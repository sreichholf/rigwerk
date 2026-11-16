//! The two `IConnectionPoint` proxies between a plugin's component and its
//! separate controller (SDK `connectionproxy.cpp`, `plugprovider.cpp`).
//!
//! Each side is connected to a host proxy, not to the other side, so the
//! host can drop a message sent from the wrong thread: `notify` is
//! forwarded only on the thread that made the connection (the UI thread),
//! as the SDK's proxy does.  Off that thread the proxy answers
//! `kResultFalse` without touching its lock, so a plugin calling from its
//! audio thread never waits.

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread::{self, ThreadId};

use ::vst3::Steinberg::Vst::{
    IComponent, IConnectionPoint, IConnectionPointTrait, IEditController, IMessage,
};
use ::vst3::Steinberg::{kResultFalse, kResultOk, tresult};
use ::vst3::{Class, ComPtr, ComWrapper};

/// The host side of one direction: what `source` sends arrives at
/// `notify` and goes on to `dest`.
pub(crate) struct ConnectionProxy {
    /// The plugin end that sees this proxy as its peer.
    source: ComPtr<IConnectionPoint>,
    /// The other plugin end, while linked.
    dest: Mutex<Option<ComPtr<IConnectionPoint>>>,
    /// The only thread messages are forwarded on.
    main_thread: ThreadId,
    /// Test hook: messages forwarded to `dest`.
    #[cfg(test)]
    forwarded: AtomicUsize,
}

impl ConnectionProxy {
    fn new(source: ComPtr<IConnectionPoint>, main_thread: ThreadId) -> Self {
        Self {
            source,
            dest: Mutex::new(None),
            main_thread,
            #[cfg(test)]
            forwarded: AtomicUsize::new(0),
        }
    }

    fn dest(&self) -> MutexGuard<'_, Option<ComPtr<IConnectionPoint>>> {
        self.dest.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Links `source` to `dest` through `proxy`: `source.connect(proxy)`
    /// (the SDK's `ConnectionProxy::connect`).
    fn link(proxy: &ComWrapper<Self>, dest: ComPtr<IConnectionPoint>) -> tresult {
        *proxy.dest() = Some(dest);
        let this = proxy
            .as_com_ref::<IConnectionPoint>()
            .expect("ConnectionProxy implements IConnectionPoint");
        // SAFETY: both are live connection points; the plugin keeps a
        // reference to the proxy if it wants one.
        let result = unsafe { proxy.source.connect(this.as_ptr()) };
        if result != kResultOk {
            let refused = proxy.dest().take();
            drop(refused);
        }
        result
    }

    /// Undoes [`Self::link`]: `source.disconnect(proxy)`, then forgets the
    /// destination.  Nothing to do when not linked.
    fn unlink(proxy: &ComWrapper<Self>) {
        if proxy.dest().is_none() {
            return;
        }
        let this = proxy
            .as_com_ref::<IConnectionPoint>()
            .expect("ConnectionProxy implements IConnectionPoint");
        // SAFETY: `source` is live and was connected to this proxy.
        unsafe { proxy.source.disconnect(this.as_ptr()) };
        // Released outside the lock: a release may run plugin code.
        let dest = proxy.dest().take();
        drop(dest);
    }
}

impl Class for ConnectionProxy {
    type Interfaces = (IConnectionPoint,);
}

impl IConnectionPointTrait for ConnectionProxy {
    /// The host owns the links: a plugin may not repoint a proxy past the
    /// thread check.
    unsafe fn connect(&self, _other: *mut IConnectionPoint) -> tresult {
        kResultFalse
    }

    unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
        kResultFalse
    }

    unsafe fn notify(&self, message: *mut IMessage) -> tresult {
        if message.is_null() || thread::current().id() != self.main_thread {
            return kResultFalse;
        }
        // Cloned out of the lock: the receiver may disconnect from inside.
        let dest = match self.dest.try_lock() {
            Ok(dest) => dest.clone(),
            Err(TryLockError::Poisoned(poison)) => poison.into_inner().clone(),
            Err(TryLockError::WouldBlock) => return kResultFalse,
        };
        match dest {
            Some(dest) => {
                #[cfg(test)]
                self.forwarded.fetch_add(1, Ordering::Relaxed);
                // SAFETY: a live connection point and the plugin's message.
                unsafe { dest.notify(message) }
            }
            None => kResultFalse,
        }
    }
}

/// A component and its separate controller, connected both ways through
/// host proxies.  Dropping it disconnects both.
pub(crate) struct Connection {
    component_side: ComWrapper<ConnectionProxy>,
    controller_side: ComWrapper<ConnectionProxy>,
}

impl Connection {
    /// Connects `component` and `controller`, component side first
    /// (`plugprovider.cpp`).  `Ok(None)` when either has no connection
    /// point, so there is nothing to connect; `Err` when one refuses.
    /// Call it on the UI thread: messages are only forwarded there.
    pub(crate) fn new(
        component: &ComPtr<IComponent>,
        controller: &ComPtr<IEditController>,
    ) -> Result<Option<Self>, String> {
        let (Some(component), Some(controller)) = (
            component.cast::<IConnectionPoint>(),
            controller.cast::<IConnectionPoint>(),
        ) else {
            return Ok(None);
        };
        Self::between(component, controller).map(Some)
    }

    /// Connects two connection points, as [`Self::new`].
    fn between(
        component: ComPtr<IConnectionPoint>,
        controller: ComPtr<IConnectionPoint>,
    ) -> Result<Self, String> {
        let main_thread = thread::current().id();
        let connection = Self {
            component_side: ComWrapper::new(ConnectionProxy::new(component.clone(), main_thread)),
            controller_side: ComWrapper::new(ConnectionProxy::new(controller.clone(), main_thread)),
        };
        // On an error `connection` drops and unlinks what was linked.
        if ConnectionProxy::link(&connection.component_side, controller) != kResultOk {
            return Err("The plugin's component refused its controller connection.".to_owned());
        }
        if ConnectionProxy::link(&connection.controller_side, component) != kResultOk {
            return Err("The plugin's controller refused its component connection.".to_owned());
        }
        Ok(connection)
    }

    /// How many messages went through, both ways.
    #[cfg(test)]
    pub(crate) fn forwarded(&self) -> usize {
        self.component_side.forwarded.load(Ordering::Relaxed)
            + self.controller_side.forwarded.load(Ordering::Relaxed)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        ConnectionProxy::unlink(&self.component_side);
        ConnectionProxy::unlink(&self.controller_side);
    }
}

#[cfg(test)]
mod tests;
