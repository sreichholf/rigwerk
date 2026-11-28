//! Host `IConnectionPoint` proxies between a component and its separate
//! controller (SDK `connectionproxy.cpp`).  `notify` forwards only on the
//! thread that connected.  Off that thread it returns `kResultFalse`
//! without the lock, so an audio-thread caller never waits.

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};
use std::thread::{self, ThreadId};

use ::vst3::Steinberg::Vst::{
    IComponent, IConnectionPoint, IConnectionPointTrait, IEditController, IMessage,
};
use ::vst3::Steinberg::{kResultFalse, kResultOk, tresult};
use ::vst3::{Class, ComPtr, ComWrapper};

/// One direction: `notify` from `source` is forwarded to `dest`.
pub(crate) struct ConnectionProxy {
    /// The plugin end that sees this proxy as its peer.
    source: ComPtr<IConnectionPoint>,
    /// The other plugin end, while linked.
    dest: Mutex<Option<ComPtr<IConnectionPoint>>>,
    /// The only thread messages are forwarded on.
    main_thread: ThreadId,
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

    /// `source.connect(proxy)`, as the SDK's `ConnectionProxy::connect`.
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
    /// The host owns the links.  A plugin cannot repoint a proxy.
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

pub(crate) struct Connection {
    component_side: ComWrapper<ConnectionProxy>,
    controller_side: ComWrapper<ConnectionProxy>,
}

impl Connection {
    /// Component side first (`plugprovider.cpp`).  `Ok(None)` when either
    /// side has no connection point.  Call on the UI thread: that is the
    /// only thread `notify` forwards on.
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
