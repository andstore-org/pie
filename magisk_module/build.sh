#!/bin/sh
set -e
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
SOURCE_DIR=${SCRIPT_DIR}/../

setup_rust() {
    if ! rustup target list --installed | grep -q "^$RUST_TARGET$"; then
        rustup target add "$RUST_TARGET" || exit 1
    fi
}


build() {
    arch="$1"
    api="$2"
    mkdir -p "$SCRIPT_DIR/bins"
    cd "$SOURCE_DIR"
    [ ! -f ./build_env.sh ] && wget "https://raw.githubusercontent.com/andstore-org/andstore-repo/main/packages/build_env.sh"
    source ./build_env.sh "$arch" "$api"
    DESTINY=$SCRIPT_DIR/bins/pie-${ARCH}
    
    setup_rust
    mkdir -p "$SOURCE_DIR/.cargo"
    cat > "$SOURCE_DIR/.cargo/config.toml" <<EOF
[target.$RUST_TARGET]
linker = "$CC_ABS"
ar = "$AR"
EOF
    cargo build --release --target "$RUST_TARGET"
    cp "$SOURCE_DIR/target/$RUST_TARGET/release/pie" "$DESTINY"
}


cd "$SCRIPT_DIR"

for arch in arm64-v8a armeabi-v7a x86 x86_64; do
    api=21
    build "$arch" "$api"
done
