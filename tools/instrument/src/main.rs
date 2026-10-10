//! Compute-module instrumentation CLI (production version of the Phase 0
//! spike tool). Pipeline stage between `cargo build` and `wasm-opt`:
//!
//! Usage: instrument <input.wasm> <output.wasm> [--max-memory-mb N]
//!
//! 1. Validates the module's import surface against the platform allowlist
//!    (the `ck` host API + the minimal deterministic WASI stub set).
//! 2. Clamps the module's linear-memory maximum to `--max-memory-mb` (worker
//!    resourceLimits do NOT cover wasm memory — this is the real cap).
//! 3. Injects mutable-global gas metering (exported global `ck_fuel`):
//!    every metered block decrements the global; on exhaustion the global is
//!    set to the u64::MAX sentinel and the module traps (`unreachable`).
//!    The host sets/reads `ck_fuel` around each entry call.

use anyhow::{bail, Context, Result};
use radix_wasm_instrument::gas_metering::{inject, mutable_global, ConstantCostRules};
use radix_wasm_instrument::utils::module_info::ModuleInfo;
use wasmparser::{Parser, Payload, TypeRef};

const WASM_PAGE_BYTES: u64 = 65536;

const ALLOWED_CK_IMPORTS: &[&str] =
    &["log", "now_ms", "state_get", "state_set", "host_call"];
const ALLOWED_WASI_IMPORTS: &[&str] = &[
    "environ_get",
    "environ_sizes_get",
    "fd_write",
    "proc_exit",
    "clock_time_get",
    "random_get",
];

fn validate_imports(wasm: &[u8]) -> Result<()> {
    for payload in Parser::new(0).parse_all(wasm) {
        if let Payload::ImportSection(reader) = payload? {
            for import in reader {
                let import = import?;
                if !matches!(import.ty, TypeRef::Func(_)) {
                    bail!("non-function import {}::{} not allowed", import.module, import.name);
                }
                let allowed = match import.module {
                    "ck" => ALLOWED_CK_IMPORTS.contains(&import.name),
                    "wasi_snapshot_preview1" => ALLOWED_WASI_IMPORTS.contains(&import.name),
                    _ => false,
                };
                if !allowed {
                    bail!("import {}::{} not in allowlist", import.module, import.name);
                }
            }
        }
    }
    Ok(())
}

/// Rewrite the memory section so every defined memory carries a maximum of at
/// most `max_pages`. Rebuilds just the memory section bytes and splices them
/// into the module (sections are independently sized, so this is safe).
fn clamp_memory(wasm: &[u8], max_pages: u64) -> Result<Vec<u8>> {
    let mut mem_section_range: Option<std::ops::Range<usize>> = None;
    let mut memories: Vec<wasmparser::MemoryType> = Vec::new();

    for payload in Parser::new(0).parse_all(wasm) {
        if let Payload::MemorySection(reader) = payload? {
            mem_section_range = Some(reader.range());
            for mem in reader {
                memories.push(mem?);
            }
        }
    }
    let Some(range) = mem_section_range else {
        // No defined memory (e.g. imported) -- nothing to clamp; the import
        // gate already rejects non-function imports, so this cannot happen
        // for accepted modules.
        return Ok(wasm.to_vec());
    };

    let mut section = wasm_encoder::MemorySection::new();
    for mem in &memories {
        if mem.memory64 || mem.shared {
            bail!("memory64/shared memories are not allowed");
        }
        if mem.initial > max_pages {
            bail!(
                "module declares initial memory of {} pages (> clamp of {} pages)",
                mem.initial,
                max_pages
            );
        }
        let clamped_max = mem.maximum.map_or(max_pages, |m| m.min(max_pages));
        section.memory(wasm_encoder::MemoryType {
            minimum: mem.initial,
            maximum: Some(clamped_max),
            memory64: false,
            shared: false,
        });
    }

    // Encode the replacement section (id byte + LEB size + payload).
    let mut replacement = Vec::new();
    wasm_encoder::Section::append_to(&section, &mut replacement);

    // `range` covers the section payload (not the id/size header). Locate the
    // header: it ends where the payload begins. The id byte for the memory
    // section is 5; scan backwards over the LEB128 size.
    let payload_start = range.start;
    let mut header_start = payload_start - 1;
    // size LEB: walk back while continuation bit set on the previous byte.
    while header_start > 0 && wasm[header_start - 1] & 0x80 != 0 {
        header_start -= 1;
    }
    header_start -= 1; // the section id byte
    if wasm[header_start] != 5 {
        bail!("failed to locate memory section header");
    }

    let mut out = Vec::with_capacity(wasm.len() + 8);
    out.extend_from_slice(&wasm[..header_start]);
    out.extend_from_slice(&replacement);
    out.extend_from_slice(&wasm[range.end..]);
    Ok(out)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let (input, output, max_memory_mb) = match args.as_slice() {
        [_, i, o] => (i.clone(), o.clone(), 64u64),
        [_, i, o, flag, n] if flag == "--max-memory-mb" => (
            i.clone(),
            o.clone(),
            n.parse::<u64>().context("parsing --max-memory-mb")?,
        ),
        _ => bail!("usage: instrument <input.wasm> <output.wasm> [--max-memory-mb N]"),
    };
    if max_memory_mb == 0 {
        bail!("--max-memory-mb must be >= 1");
    }
    let max_pages = (max_memory_mb * 1024 * 1024) / WASM_PAGE_BYTES;

    let wasm = std::fs::read(&input).with_context(|| format!("reading {input}"))?;
    validate_imports(&wasm)?;
    let clamped = clamp_memory(&wasm, max_pages)?;
    wasmparser::validate(&clamped).context("validating memory-clamped module")?;

    let mut info =
        ModuleInfo::new(&clamped).map_err(|e| anyhow::anyhow!("parsing module: {e:?}"))?;
    let backend = mutable_global::Injector::new("env", "ck_fuel");
    // Uniform cost of 1 per instruction; production per-instruction weights
    // are Phase 10 load-test work (the fuel scale is calibrated in policy).
    let instrumented = inject(&mut info, backend, &ConstantCostRules::default())
        .context("injecting gas metering")?;

    wasmparser::validate(&instrumented).context("validating instrumented module")?;

    std::fs::write(&output, &instrumented).with_context(|| format!("writing {output}"))?;
    eprintln!(
        "instrumented {} ({} bytes) -> {} ({} bytes, +{:.1}%, memory max {} pages)",
        input,
        wasm.len(),
        output,
        instrumented.len(),
        (instrumented.len() as f64 / wasm.len() as f64 - 1.0) * 100.0,
        max_pages
    );
    Ok(())
}
