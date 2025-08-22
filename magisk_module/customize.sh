ARCH="$(getprop ro.product.cpu.abi)"
PATH="/data/local/andstore"
MKSHRC="${MODPATH}/system/etc/mkshrc"

[ ! -f "${MODPATH}/bins/pie-${ARCH}" ] && abort "- Unsupported Architecture"

mkdir -p "${MODPATH}/system/bin"
mkdir -p "${PATH}/bin"
mkdir -p "${PATH}/lib"

echo "$ARCH" | grep -q "64" && mkdir -p "$PATH/lib64"

cp "${MODPATH}/bins/pie-${ARCH}" "${MODPATH}/system/bin/pie"

[ -f /system/etc/mkshrc ] && mkdir -p "${MODPATH}/system/etc" && cp "/system/etc/mkshrc" "${MODPATH}/system/etc/"

grep -q "/data/local/andstore/bin" "$MKSHRC" || echo "export PATH=\$PATH:${PATH}/bin" >> "$MKSHRC"

for libdir in "$PATH/lib" "$PATH/lib64"; do
    [ -d "$libdir" ] && grep -q "$libdir" "$MKSHRC" || echo "export LD_LIBRARY_PATH=\$LD_LIBRARY_PATH:${libdir}" >> "$MKSHRC"
done

rm -rf ${MODPATH}/bins

set_perm_recursive $MODPATH/system 0 0 0755 0644
