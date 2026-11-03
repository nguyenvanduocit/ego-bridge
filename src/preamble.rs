//! The JavaScript preamble that maps caller paths into the run's mirror.

use std::path::Path;

const TEMPLATE: &str = include_str!("preamble.mjs");
const ROOT_PLACEHOLDER: &str = "__EGO_BRIDGE_MIRROR_ROOT__";
const MODULE_PLACEHOLDER: &str = "__EGO_BRIDGE_SCRIPT_MODULE__";

/// Preamble source for a run whose mirror lives at `mirror_root`.
/// `mirror_root` must be canonical so `realpath` results map back.
/// `script_module` is the `file://` URL of the caller's script when it runs
/// as a module file, so that its imports are mapped too.
pub fn render(
    mirror_root: &Path,
    script_module: Option<&str>,
) -> Result<String, serde_json::Error> {
    let root = serde_json::to_string(&mirror_root.to_string_lossy())?;
    let module = serde_json::to_string(&script_module)?;
    Ok(TEMPLATE
        .replacen(ROOT_PLACEHOLDER, &root, 1)
        .replacen(MODULE_PLACEHOLDER, &module, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_the_root_as_a_js_string_literal() -> Result<(), serde_json::Error> {
        let source = render(Path::new("/srv/runs/a \"b\"/fs"), None)?;
        assert!(source.contains(r#"const ROOT = "/srv/runs/a \"b\"/fs";"#));
        assert!(source.contains("const SCRIPT_MODULE = null;"));
        assert!(!source.contains(ROOT_PLACEHOLDER));
        assert!(!source.contains(MODULE_PLACEHOLDER));

        let module = render(Path::new("/r"), Some("file:///r/script.mjs"))?;
        assert!(module.contains(r#"const SCRIPT_MODULE = "file:///r/script.mjs";"#));
        Ok(())
    }
}
