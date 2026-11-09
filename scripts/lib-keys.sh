# Sourced by the bootstrap scripts: a project-local development signing key.
#
# Everything the scripts publish is signed with it, and every build trusts only it.
# The key lives in keys/ (git-ignored) and is created on first use. It is a DEVELOPMENT
# key: anyone with this checkout's keys/ directory can sign for these repositories.
KEYS=${KEYS:-keys}
SIGN=(--sign "$KEYS/signing/bootstrap.key")
TRUST=(--keys "$KEYS/trusted")

ensure_dev_key() {
    if [ ! -f "$KEYS/signing/bootstrap.key" ]; then
        "$FORGE" key generate bootstrap --dir "$KEYS/signing" >/dev/null || return 1
    fi
    "$FORGE" key trust "$KEYS/signing/bootstrap.pub" --dir "$KEYS/trusted" >/dev/null
}

# Sign a repository that already exists (e.g. one built before signing was added).
sign_existing_repo() {
    local repo=$1
    if [ -f "$repo/index.toml" ] && [ ! -f "$repo/index.toml.sig" ]; then
        "$FORGE" repo sign "$repo" --key "$KEYS/signing/bootstrap.key"
    fi
}
