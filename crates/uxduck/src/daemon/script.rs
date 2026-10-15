//! TypeScript plugins (ADR-0006 X7): the daemon strips a script's types
//! natively, with oxc, and the script host built into UXDuck runs the JavaScript in
//! the plugin sandbox. Nothing is compiled to WebAssembly per plugin.

use std::path::Path;

use oxc::allocator::Allocator;
use oxc::codegen::Codegen;
use oxc::parser::Parser;
use oxc::semantic::SemanticBuilder;
use oxc::span::SourceType;
use oxc::transformer::{TransformOptions, Transformer};

/// A script's JavaScript: its types and type-only imports removed, and
/// nothing else changed.
///
/// # Errors
/// When it does not parse, with the first error.
pub fn strip_types(path: &Path, source: &str) -> Result<String, String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path)
        .unwrap_or_else(|_| SourceType::ts())
        .with_module(true);
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if let Some(error) = parsed.diagnostics.first() {
        return Err(format!("{}: {error}", path.display()));
    }
    let mut program = parsed.program;
    let scoping = SemanticBuilder::new()
        .build(&program)
        .semantic
        .into_scoping();
    let transformed = Transformer::new(&allocator, path, &TransformOptions::default())
        .build_with_scoping(scoping, &mut program);
    if let Some(error) = transformed.diagnostics.first() {
        return Err(format!("{}: {error}", path.display()));
    }
    Ok(Codegen::new().build(&program).code)
}

/// The script host, built into the executable (`build.rs`): an installer
/// cannot leave it behind.
pub const HOST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/script_host.wasm"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_go_and_the_code_stays() {
        let script = r#"
import type { ItemBatch, Effects } from "@uxduck/sdk";
import { value } from "@uxduck/sdk";
interface Held { count: number }
const held: Held = { count: 0 };
export function onItems(batch: ItemBatch): Effects {
  held.count += batch.items.length as number;
  return { items: [value("v", "a.b_m", "A", "meter", held.count)] };
}
"#;
        let js = strip_types(Path::new("index.ts"), script).unwrap();
        assert!(!js.contains("interface"), "{js}");
        assert!(!js.contains("ItemBatch"), "{js}");
        assert!(!js.contains(": Held"), "{js}");
        assert!(
            js.contains(r#"import { value } from "@uxduck/sdk""#),
            "{js}"
        );
        assert!(js.contains("export function onItems(batch)"), "{js}");
        let broken = strip_types(Path::new("index.ts"), "export function (").unwrap_err();
        assert!(broken.starts_with("index.ts: "), "{broken}");
    }
}
