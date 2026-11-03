//! The JavaScript preamble that maps caller paths into the run's mirror.

use std::path::Path;

const TEMPLATE: &str = include_str!("preamble.mjs");
const PLACEHOLDER: &str = "__EGO_BRIDGE_MIRROR_ROOT__";

/// Preamble source for a run whose mirror lives at `mirror_root`.
/// `mirror_root` must be canonical so `realpath` results map back.
pub fn render(mirror_root: &Path) -> Result<String, serde_json::Error> {
    let root = serde_json::to_string(&mirror_root.to_string_lossy())?;
    Ok(TEMPLATE.replacen(PLACEHOLDER, &root, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_the_root_as_a_js_string_literal() -> Result<(), serde_json::Error> {
        let source = render(Path::new("/srv/runs/a \"b\"/fs"))?;
        assert!(source.contains(r#"const ROOT = "/srv/runs/a \"b\"/fs";"#));
        assert!(!source.contains(PLACEHOLDER));
        Ok(())
    }
}
