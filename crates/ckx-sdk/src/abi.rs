//! Host imports (see `docs/GUEST-ABI.md`). Outside wasm32 they are stubs so node logic that
//! makes no calls can be unit tested natively.

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

#[cfg(not(target_arch = "wasm32"))]
pub fn emit(_inject: &[u8]) -> u32 {
    0
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

#[cfg(not(target_arch = "wasm32"))]
pub fn wake_at(_at_ms: u64) {}

#[cfg(not(target_arch = "wasm32"))]
pub fn watch(_req: &[u8], _on: bool) -> u64 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
pub fn persist_now() {}

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

#[cfg(not(target_arch = "wasm32"))]
pub fn log(level: u32, msg: &str) {
    eprintln!("[ckx level {level}] {msg}");
}

#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn call(_req: &[u8], _deadline_ms: u32) -> u64 {
    0
}

#[cfg(not(target_arch = "wasm32"))]
pub fn send(_req: &[u8]) -> u32 {
    8
}

#[cfg(not(target_arch = "wasm32"))]
pub fn publish(_topic: &str, _payload: &[u8]) {}

#[cfg(not(target_arch = "wasm32"))]
pub fn node(_op: &str, _payload: &[u8], _deadline_ms: u32) -> u64 {
    0
}
