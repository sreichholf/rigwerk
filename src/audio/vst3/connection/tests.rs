use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ::vst3::Steinberg::Vst::{IConnectionPoint, IConnectionPointTrait, IMessage};
use ::vst3::Steinberg::{kResultFalse, kResultOk, kResultTrue, tresult};
use ::vst3::{Class, ComPtr, ComWrapper};

use super::{Connection, ConnectionProxy};
use crate::audio::vst3::message::HostMessage;

/// A plugin end that counts what the host does to it.
#[derive(Default)]
struct End {
    connects: AtomicUsize,
    disconnects: AtomicUsize,
    notified: AtomicUsize,
    /// Refuse every `connect`.
    refuse: AtomicBool,
}

impl Class for End {
    type Interfaces = (IConnectionPoint,);
}

impl IConnectionPointTrait for End {
    unsafe fn connect(&self, _other: *mut IConnectionPoint) -> tresult {
        self.connects.fetch_add(1, Ordering::SeqCst);
        if self.refuse.load(Ordering::SeqCst) {
            kResultFalse
        } else {
            kResultTrue
        }
    }

    unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
        self.disconnects.fetch_add(1, Ordering::SeqCst);
        kResultTrue
    }

    unsafe fn notify(&self, _message: *mut IMessage) -> tresult {
        self.notified.fetch_add(1, Ordering::SeqCst);
        kResultOk
    }
}

fn point(end: &ComWrapper<End>) -> ComPtr<IConnectionPoint> {
    end.to_com_ptr::<IConnectionPoint>()
        .expect("a connection point")
}

#[test]
fn proxy_forwards_messages_on_the_main_thread_only() {
    let source = ComWrapper::new(End::default());
    let dest = ComWrapper::new(End::default());
    let proxy = ComWrapper::new(ConnectionProxy::new(
        point(&source),
        std::thread::current().id(),
    ));
    assert_eq!(ConnectionProxy::link(&proxy, point(&dest)), kResultOk);
    assert_eq!(source.connects.load(Ordering::SeqCst), 1);

    let message = ComWrapper::new(HostMessage::new());
    let message = message.to_com_ptr::<IMessage>().expect("a message");
    let link = proxy.to_com_ptr::<IConnectionPoint>().expect("the proxy");
    // SAFETY: a live message through a live proxy.
    assert_eq!(unsafe { link.notify(message.as_ptr()) }, kResultOk);
    assert_eq!(dest.notified.load(Ordering::SeqCst), 1);

    // From another thread (a plugin's audio thread): dropped.
    let (other, sent) = (link.clone(), message.clone());
    let result = std::thread::spawn(move || {
        // SAFETY: as above.
        unsafe { other.notify(sent.as_ptr()) }
    })
    .join()
    .expect("the thread");
    assert_eq!(result, kResultFalse);
    assert_eq!(dest.notified.load(Ordering::SeqCst), 1);

    // A plugin may not repoint the proxy.
    // SAFETY: a live proxy and end.
    assert_eq!(unsafe { link.connect(point(&dest).as_ptr()) }, kResultFalse);

    ConnectionProxy::unlink(&proxy);
    assert_eq!(source.disconnects.load(Ordering::SeqCst), 1);
    // SAFETY: as above.
    assert_eq!(unsafe { link.notify(message.as_ptr()) }, kResultFalse);
    assert_eq!(dest.notified.load(Ordering::SeqCst), 1);
    // Unlinking twice disconnects once.
    ConnectionProxy::unlink(&proxy);
    assert_eq!(source.disconnects.load(Ordering::SeqCst), 1);
}

#[test]
fn connection_undoes_the_first_link_when_the_second_is_refused() {
    let component = ComWrapper::new(End::default());
    let controller = ComWrapper::new(End::default());
    controller.refuse.store(true, Ordering::SeqCst);
    assert!(Connection::between(point(&component), point(&controller)).is_err());
    // The component side was linked, then unlinked once.
    assert_eq!(component.connects.load(Ordering::SeqCst), 1);
    assert_eq!(component.disconnects.load(Ordering::SeqCst), 1);
    // The refused side is never disconnected.
    assert_eq!(controller.connects.load(Ordering::SeqCst), 1);
    assert_eq!(controller.disconnects.load(Ordering::SeqCst), 0);

    // Both accept: linked both ways, unlinked both ways on drop.
    controller.refuse.store(false, Ordering::SeqCst);
    let connection = Connection::between(point(&component), point(&controller)).expect("connected");
    assert_eq!(component.connects.load(Ordering::SeqCst), 2);
    assert_eq!(controller.connects.load(Ordering::SeqCst), 2);
    drop(connection);
    assert_eq!(component.disconnects.load(Ordering::SeqCst), 2);
    assert_eq!(controller.disconnects.load(Ordering::SeqCst), 1);
}
