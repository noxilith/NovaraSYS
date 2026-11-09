//! Round-trip editing of declaration files.
//!
//! `forge pkg add/remove` edits the `packages` list in place, preserving the
//! user's comments and layout, so the declaration stays the single source of
//! truth.

use toml_edit::{Array, DocumentMut, Item, Value};

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("invalid TOML")]
    Parse(#[from] toml_edit::TomlError),
    #[error("`packages` exists but is not an array of strings")]
    BadPackages,
    #[error("invalid package name `{0}`")]
    BadName(String),
}

/// Outcome of an edit: the new file text and whether anything changed.
#[derive(Debug, PartialEq, Eq)]
pub struct Edited {
    pub text: String,
    pub changed: bool,
}

fn check_name(name: &str) -> Result<(), EditError> {
    let ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'));
    if ok { Ok(()) } else { Err(EditError::BadName(name.into())) }
}

fn packages_mut(doc: &mut DocumentMut) -> Result<&mut Array, EditError> {
    let item = doc.entry("packages").or_insert(Item::Value(Value::Array(Array::new())));
    let arr = item.as_array_mut().ok_or(EditError::BadPackages)?;
    if arr.iter().any(|v| !v.is_str()) {
        return Err(EditError::BadPackages);
    }
    Ok(arr)
}

/// Add `name` to `packages` (no-op if already present).
pub fn add_package(text: &str, name: &str) -> Result<Edited, EditError> {
    check_name(name)?;
    let mut doc: DocumentMut = text.parse()?;
    let arr = packages_mut(&mut doc)?;
    if arr.iter().any(|v| v.as_str() == Some(name)) {
        return Ok(Edited { text: text.to_string(), changed: false });
    }
    arr.push(name);
    Ok(Edited { text: doc.to_string(), changed: true })
}

/// Remove `name` from `packages` (no-op if absent).
pub fn remove_package(text: &str, name: &str) -> Result<Edited, EditError> {
    check_name(name)?;
    let mut doc: DocumentMut = text.parse()?;
    if doc.get("packages").is_none() {
        return Ok(Edited { text: text.to_string(), changed: false });
    }
    let arr = packages_mut(&mut doc)?;
    let Some(idx) = arr.iter().position(|v| v.as_str() == Some(name)) else {
        return Ok(Edited { text: text.to_string(), changed: false });
    };
    arr.remove(idx);
    Ok(Edited { text: doc.to_string(), changed: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_schema::UserDecl;

    #[test]
    fn adds_to_empty_file() {
        let e = add_package("", "kitty").unwrap();
        assert!(e.changed);
        assert_eq!(UserDecl::parse(&e.text).unwrap().packages, ["kitty"]);
    }

    #[test]
    fn add_is_idempotent() {
        let first = add_package("", "kitty").unwrap();
        let second = add_package(&first.text, "kitty").unwrap();
        assert!(!second.changed);
        assert_eq!(second.text, first.text);
    }

    #[test]
    fn preserves_comments_and_other_keys() {
        let src = "# my machine\nhostname = \"novara\" # name\n\npackages = [\"a\"] # apps\n";
        let e = add_package(src, "b").unwrap();
        assert!(e.text.contains("# my machine"));
        assert!(e.text.contains("# name"));
        assert!(e.text.contains("# apps"));
        assert!(e.text.contains("hostname = \"novara\""));
        assert!(e.text.contains("\"b\""));
    }

    #[test]
    fn removes_package() {
        let e = remove_package("packages = [\"a\", \"b\"]\n", "a").unwrap();
        assert!(e.changed);
        assert_eq!(UserDecl::parse(&e.text).unwrap().packages, ["b"]);
    }

    #[test]
    fn remove_missing_is_noop() {
        assert!(!remove_package("packages = [\"a\"]\n", "z").unwrap().changed);
        assert!(!remove_package("hostname = \"x\"\n", "z").unwrap().changed);
    }

    #[test]
    fn rejects_bad_names_and_shapes() {
        assert!(matches!(add_package("", "a b"), Err(EditError::BadName(_))));
        assert!(matches!(add_package("", ""), Err(EditError::BadName(_))));
        assert!(matches!(add_package("packages = 3\n", "a"), Err(EditError::BadPackages)));
        assert!(matches!(add_package("packages = [1]\n", "a"), Err(EditError::BadPackages)));
    }
}
