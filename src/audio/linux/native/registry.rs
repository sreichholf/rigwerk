//! `list_nodes` is one shot, so a new node would wait for the slow poll.
//! RigWerk's own nodes are not endpoints, so starting the bridge does not
//! wake a scan.  Nodes present at connect are only recorded; a reconnect
//! wakes once, because the graph may have changed.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use ::pipewire as pw;

use super::{endpoint_kind, ensure_init, EndpointKind};

/// Retry when no server is running or the connection was lost.
const RETRY: Duration = Duration::from_secs(5);

/// Same filter as `list_nodes`.
pub(crate) fn props_endpoint<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<EndpointKind> {
    let name = get("node.name")?;
    let class = get("media.class").unwrap_or("");
    let monitor = get("stream.monitor") == Some("true");
    endpoint_kind(name, class, monitor)
}

/// Without a server the thread retries, and the caller keeps its slow poll.
/// One process-wide watch is enough.  The thread ends once the receiver is
/// dropped and the next event finds it gone.
pub(crate) fn spawn() -> Receiver<()> {
    let (sender, receiver) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("rigwerk-pw-registry".to_owned())
        .spawn(move || {
            ensure_init();
            let mut announce = false;
            while watch(&sender, announce) {
                announce = true;
                thread::sleep(RETRY);
            }
        });
    receiver
}

#[derive(Default)]
struct State {
    /// Registry ids seen, so a removal can be matched.
    endpoints: HashSet<u32>,
    /// After the first sync, arrivals are changes.
    synced: bool,
    /// The receiver is gone.
    closed: bool,
}

/// `false` once the receiver is gone; otherwise connect again.
fn watch(sender: &Sender<()>, announce: bool) -> bool {
    let Ok(mainloop) = pw::main_loop::MainLoopRc::new(None) else {
        return true;
    };
    let Ok(context) = pw::context::ContextRc::new(&mainloop, None) else {
        return true;
    };
    let Ok(core) = context.connect_rc(None) else {
        return true;
    };
    let Ok(registry) = core.get_registry_rc() else {
        return true;
    };
    let Ok(pending) = core.sync(0) else {
        return true;
    };

    let state = Rc::new(RefCell::new(State::default()));
    let wake: Rc<dyn Fn()> = {
        let state = Rc::clone(&state);
        let mainloop = mainloop.clone();
        let sender = sender.clone();
        Rc::new(move || {
            if sender.send(()).is_err() {
                state.borrow_mut().closed = true;
                mainloop.quit();
            }
        })
    };

    let _core_listener = core
        .add_listener_local()
        .done({
            let state = Rc::clone(&state);
            let wake = Rc::clone(&wake);
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    state.borrow_mut().synced = true;
                    if announce {
                        wake();
                    }
                }
            }
        })
        .error({
            let mainloop = mainloop.clone();
            move |id, _seq, _res, _message| {
                // A core error is the connection going away (EPIPE when the
                // server exits).
                if id == pw::core::PW_ID_CORE {
                    mainloop.quit();
                }
            }
        })
        .register();
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let state = Rc::clone(&state);
            let wake = Rc::clone(&wake);
            move |global| {
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Some(props) = global.props.as_ref() else {
                    return;
                };
                if props_endpoint(|key| props.get(key)).is_none() {
                    return;
                }
                let synced = {
                    let mut state = state.borrow_mut();
                    state.endpoints.insert(global.id);
                    state.synced
                };
                if synced {
                    wake();
                }
            }
        })
        .global_remove({
            let state = Rc::clone(&state);
            let wake = Rc::clone(&wake);
            move |id| {
                let removed = {
                    let mut state = state.borrow_mut();
                    state.endpoints.remove(&id) && state.synced
                };
                if removed {
                    wake();
                }
            }
        })
        .register();

    mainloop.run();
    let closed = state.borrow().closed;
    !closed
}

#[cfg(test)]
mod tests;
