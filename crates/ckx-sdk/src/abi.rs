//! Host imports: what the sandbox gives a module. Outside wasm32 they reach the host a native
//! test puts on its thread ([`crate::testing`]); without one, logs go to stderr, the clock is
//! the system's and nothing else answers.

#[cfg(target_arch = "wasm32")]
mod host {
    #[link(wasm_import_module = "ckx")]
    unsafe extern "C" {
        pub fn log(level: u32, ptr: u32, len: u32);
        pub fn now_ms() -> u64;
        pub fn call(req_ptr: u32, req_len: u32, deadline_ms: u32) -> u64;
        pub fn send(req_ptr: u32, req_len: u32) -> u32;
        pub fn publish(topic_ptr: u32, topic_len: u32, payload_ptr: u32, payload_len: u32);
        pub fn node(op_ptr: u32, op_len: u32, payload_ptr: u32, payload_len: u32, deadline_ms: u32) -> u64;
        pub fn wake_at(at_ms: u64);
        pub fn subscribe(req_ptr: u32, req_len: u32) -> u64;
        pub fn unsubscribe(req_ptr: u32, req_len: u32) -> u64;
        pub fn persist_now();
        pub fn emit(ptr: u32, len: u32) -> u32;
    }
}

#[cfg(target_arch = "wasm32")]
pub fn emit(inject: &[u8]) -> u32 {
    unsafe { host::emit(inject.as_ptr() as u32, inject.len() as u32) }
}

#[cfg(target_arch = "wasm32")]
pub fn wake_at(at_ms: u64) {
    unsafe { host::wake_at(at_ms) }
}

#[cfg(target_arch = "wasm32")]
pub fn watch(req: &[u8], on: bool) -> u64 {
    let (ptr, len) = (req.as_ptr() as u32, req.len() as u32);
    unsafe { if on { host::subscribe(ptr, len) } else { host::unsubscribe(ptr, len) } }
}

#[cfg(target_arch = "wasm32")]
pub fn persist_now() {
    unsafe { host::persist_now() }
}

#[cfg(target_arch = "wasm32")]
pub fn log(level: u32, msg: &str) {
    unsafe { host::log(level, msg.as_ptr() as u32, msg.len() as u32) }
}

#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> u64 {
    unsafe { host::now_ms() }
}

#[cfg(target_arch = "wasm32")]
pub fn call(req: &[u8], deadline_ms: u32) -> u64 {
    unsafe { host::call(req.as_ptr() as u32, req.len() as u32, deadline_ms) }
}

#[cfg(target_arch = "wasm32")]
pub fn send(req: &[u8]) -> u32 {
    unsafe { host::send(req.as_ptr() as u32, req.len() as u32) }
}

#[cfg(target_arch = "wasm32")]
pub fn publish(topic: &str, payload: &[u8]) {
    unsafe { host::publish(topic.as_ptr() as u32, topic.len() as u32, payload.as_ptr() as u32, payload.len() as u32) }
}

#[cfg(target_arch = "wasm32")]
pub fn node(op: &str, payload: &[u8], deadline_ms: u32) -> u64 {
    unsafe { host::node(op.as_ptr() as u32, op.len() as u32, payload.as_ptr() as u32, payload.len() as u32, deadline_ms) }
}

/// The host a native build calls instead of the sandbox. Imports that return a result answer
/// `[status][payload]`, as the sandbox writes it into the module's memory.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod native {
    use std::cell::RefCell;

    /// Each import answers as it does with no host unless the host says otherwise.
    pub(crate) trait Host {
        fn log(&mut self, level: u32, msg: &str) {
            eprintln!("[ckx level {level}] {msg}");
        }
        fn now_ms(&mut self) -> u64 {
            super::system_ms()
        }
        fn call(&mut self, _req: &[u8], _deadline_ms: u32) -> Vec<u8> {
            Vec::new()
        }
        fn send(&mut self, _req: &[u8]) -> u32 {
            8
        }
        fn publish(&mut self, _topic: &str, _payload: &[u8]) {}
        fn node(&mut self, _op: &str, _payload: &[u8], _deadline_ms: u32) -> Vec<u8> {
            Vec::new()
        }
        fn wake_at(&mut self, _at_ms: u64) {}
        fn watch(&mut self, _req: &[u8], _on: bool) -> Vec<u8> {
            Vec::new()
        }
        fn persist_now(&mut self) {}
        fn emit(&mut self, _inject: &[u8]) -> u32 {
            0
        }
    }

    thread_local! {
        static HOST: RefCell<Option<Box<dyn Host>>> = const { RefCell::new(None) };
        /// The result the last import returned, which `rt::import_result` takes at once.
        static RESULT: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    }

    /// Puts `host` on this thread (or takes it off) and returns the one it replaces.
    pub(crate) fn install(host: Option<Box<dyn Host>>) -> Option<Box<dyn Host>> {
        HOST.with(|h| std::mem::replace(&mut *h.borrow_mut(), host))
    }

    pub(crate) fn with<R>(f: impl FnOnce(&mut (dyn Host + 'static)) -> R) -> Option<R> {
        HOST.with(|h| h.borrow_mut().as_deref_mut().map(f))
    }

    /// Keeps an import's result for `rt::import_result`: 0 is no reply, as from the sandbox.
    pub(crate) fn result(buf: Option<Vec<u8>>) -> u64 {
        match buf {
            Some(b) if !b.is_empty() => {
                RESULT.with(|r| *r.borrow_mut() = Some(b));
                1
            }
            _ => 0,
        }
    }

    pub(crate) fn take_result() -> Option<Vec<u8>> {
        RESULT.with(|r| r.borrow_mut().take())
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn emit(inject: &[u8]) -> u32 {
    native::with(|h| h.emit(inject)).unwrap_or(0)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn wake_at(at_ms: u64) {
    native::with(|h| h.wake_at(at_ms));
}

#[cfg(not(target_arch = "wasm32"))]
pub fn watch(req: &[u8], on: bool) -> u64 {
    native::result(native::with(|h| h.watch(req, on)))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn persist_now() {
    native::with(|h| h.persist_now());
}

#[cfg(not(target_arch = "wasm32"))]
fn system_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn log(level: u32, msg: &str) {
    if native::with(|h| h.log(level, msg)).is_none() {
        eprintln!("[ckx level {level}] {msg}");
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> u64 {
    native::with(|h| h.now_ms()).unwrap_or_else(system_ms)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn call(req: &[u8], deadline_ms: u32) -> u64 {
    native::result(native::with(|h| h.call(req, deadline_ms)))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn send(req: &[u8]) -> u32 {
    native::with(|h| h.send(req)).unwrap_or(8)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn publish(topic: &str, payload: &[u8]) {
    native::with(|h| h.publish(topic, payload));
}

#[cfg(not(target_arch = "wasm32"))]
pub fn node(op: &str, payload: &[u8], deadline_ms: u32) -> u64 {
    native::result(native::with(|h| h.node(op, payload, deadline_ms)))
}
