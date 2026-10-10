use crowdy_client_sdk as crowdy;

fn init() {
    // Runs once in each visitor's browser, after they consent to this CLIENT half
    // (or trust you) and while they stand in this grid.
    crowdy::log(1, "ready");
}

fn tick(_dt_ms: u32) {
    // Every tick_interval_ms (Cargo.toml). The state blob lasts as long as the page's worker.
    let ticks = u32::from_le_bytes(crowdy::state_get().try_into().unwrap_or([0; 4])).wrapping_add(1);
    crowdy::state_set(&ticks.to_le_bytes());
    // Host calls cross the page's broker as the visiting player, inside this grid: the HUD and
    // overlay, chunk and actor reads, voxel writes, spatial and channel sends, grid events.
    // Type "crowdy::api::" for the rest. The page's DOM and tokens are never reachable.
    let _ = crowdy::api::hud_set(serde_json::json!({ "text": format!("Ticks here: {ticks}") }));
    //
    // Mouse (holodeck canvas only; Studio chrome is omitted). Drain every tick:
    //   let data = crowdy::api::pointer_clicks().unwrap_or(serde_json::json!({}));
    // data["clicks"] = [{ "t": "down"|"up", "button": 0, "atMs", "heldMs", "nx", "ny" }]
}

fn invoke(payload: &[u8]) -> Vec<u8> {
    payload.to_vec()
}

fn event(_payload: &[u8]) {
    // Grid events from the other CLIENT halves on this page (crowdy::api::emit_event).
}

crowdy::register_module!(init: init, tick: tick, invoke: invoke, event: event);
