#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#
# Rootless verification for the system install surface. Renders
# scripts/install-system.sh into temporary roots, checks the expected files,
# modes, profile selection, dry-run and --force behaviour, rejects secrets and
# world-writable paths, and validates udev/AppArmor syntax when those tools are
# available. No sudo and no host changes.
#
# Usage: scripts/verify-system-install.sh [--keep]
# Exit status: 0 all checks passed; 1 one or more failed; 2 usage error.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALL="$SCRIPT_DIR/install-system.sh"
VERIFY_UNITS="$SCRIPT_DIR/systemd/verify-units.sh"

KEEP=0
case "${1:-}" in
    "") ;;
    --keep) KEEP=1 ;;
    -h|--help)
        sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "unknown argument: $1" >&2
        exit 2
        ;;
esac

TMP=$(mktemp -d "${TMPDIR:-/tmp}/ozma-system-verify.XXXXXX")
cleanup() {
    if [ "$KEEP" -eq 1 ]; then
        printf 'KEEP  %s\n' "$TMP"
    else
        rm -rf "$TMP"
    fi
}
trap cleanup EXIT

PASS=0
FAIL=0
SKIP=0
pass() { PASS=$((PASS + 1)); printf 'PASS  %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); printf 'FAIL  %s\n' "$*" >&2; }
skip() { SKIP=$((SKIP + 1)); printf 'SKIP  %s\n' "$*"; }
section() { printf '\n== %s ==\n' "$*"; }

check_file() {
    if [ -f "$1" ]; then pass "file: ${1#"$TMP"/}"; else fail "missing file: ${1#"$TMP"/}"; fi
}
check_absent() {
    if [ -e "$1" ]; then fail "unexpected path: ${1#"$TMP"/}"; else pass "absent: ${1#"$TMP"/}"; fi
}
check_mode() {
    path=$1
    want=$2
    got=$(stat -c '%a' "$path" 2>/dev/null || true)
    if [ "$got" = "$want" ]; then
        pass "mode $want: ${path#"$TMP"/}"
    else
        fail "mode ${got:-missing} (want $want): ${path#"$TMP"/}"
    fi
}

run_install() {
    log=$1
    shift
    if "$INSTALL" "$@" >"$log" 2>&1; then
        return 0
    fi
    cat "$log" >&2
    return 1
}

# Same, but for an invocation that is expected to fail (for example a
# tampered file without --force); do not dump the expected error to stderr.
run_install_quiet() {
    log=$1
    shift
    "$INSTALL" "$@" >"$log" 2>&1
}

group_exists() { getent group "$1" >/dev/null 2>&1; }
profile_groups() {
    case "$1" in
        hardware) echo "video input render" ;;
        vm) echo "input render kvm libvirt" ;;
        desktop) echo "input video render" ;;
        all) echo "video input render kvm libvirt" ;;
        *) echo "" ;;
    esac
}

# Compare the usermod line with the groups that exist on this host. Missing
# conventional groups are intentionally omitted by the installer.
check_profile_groups() {
    profile=$1
    file=$2
    if ! command -v getent >/dev/null 2>&1; then
        skip "profile $profile groups (getent unavailable)"
        return
    fi
    want=""
    for g in $(profile_groups "$profile"); do
        group_exists "$g" && want="${want:+$want,}$g"
    done
    got=$(sed -n 's/^usermod -aG \([^ ]*\) .*/\1/p' "$file" 2>/dev/null | head -n1)
    want_sorted=$(printf '%s\n' "$want" | tr ',' '\n' | sed '/^$/d' | sort)
    got_sorted=$(printf '%s\n' "$got" | tr ',' '\n' | sed '/^$/d' | sort)
    if [ "$want_sorted" = "$got_sorted" ]; then
        pass "profile $profile groups: ${want:-none}"
    else
        fail "profile $profile groups: got '${got:-none}', want '${want:-none}'"
    fi
}

# ---------------------------------------------------------------------------
section "Shell syntax"
for script in "$INSTALL" "$VERIFY_UNITS" "$SCRIPT_DIR/verify-system-install.sh"; do
    if bash -n "$script" 2>/dev/null; then
        pass "bash -n ${script#"$REPO_ROOT"/}"
    else
        fail "bash -n ${script#"$REPO_ROOT"/}"
    fi
done

# The system installer and its runtime assets must not depend on the Python
# trees; the Python implementation is the rollback path, not an install input.
if grep -qE '\bpython[0-9.]*\b|softnode/' "$INSTALL"; then
    fail "install-system.sh references Python/softnode"
else
    pass "install-system.sh is Python-free"
fi

# ---------------------------------------------------------------------------
section "Render all profiles into a temp root"
R="$TMP/root"
if run_install "$TMP/install-all.log" --root "$R" --prefix /usr/local --profile all --usb-gadget; then
    pass "install-system.sh --profile all --usb-gadget"
else
    fail "install-system.sh --profile all --usb-gadget"
fi

for rel in \
    usr/local/lib/systemd/system/ozma-node@.service \
    usr/local/lib/systemd/system/ozma-node-vm@.service \
    usr/local/lib/systemd/system/ozma-virtual-node.service \
    usr/local/lib/systemd/system/ozma-node@.service.d/10-usb-gadget.conf \
    etc/ozma/libvirt-qemu-cgroup_device_acl.conf \
    etc/ozma/install-operator-commands.txt \
    etc/udev/rules.d/70-ozma-node-uinput.rules \
    etc/udev/rules.d/70-ozma-node-input.rules \
    etc/udev/rules.d/70-ozma-node-hidg.rules \
    etc/udev/rules.d/70-ozma-node-v4l2.rules \
    etc/udev/rules.d/70-ozma-node-render.rules \
    etc/udev/rules.d/70-ozma-node-gadget.rules \
    etc/apparmor.d/abstractions/libvirt-qemu.d/ozma \
    etc/tmpfiles.d/ozma-node.conf \
    etc/modules-load.d/ozma-node.conf; do
    check_file "$R/$rel"
done

env_count=$(find "$R/etc/ozma" -maxdepth 1 -name '*.env.example' -type f 2>/dev/null | wc -l)
if [ "$env_count" -eq 4 ]; then
    pass "all 4 env examples rendered"
else
    fail "expected 4 env examples, found $env_count"
fi

if grep -RIn '@[A-Z_][A-Z_]*@' "$R" >/dev/null 2>&1; then
    fail "unresolved @PLACEHOLDER@ in the rendered tree"
else
    pass "no unresolved @PLACEHOLDER@ values"
fi

if grep -Fq "ExecStart=$R/usr/local/bin/ozma-node --profile %i" \
    "$R/usr/local/lib/systemd/system/ozma-node@.service" &&
   grep -Fq "EnvironmentFile=-$R/etc/ozma/ozma-node-%i.env" \
    "$R/usr/local/lib/systemd/system/ozma-node@.service"; then
    pass "unit paths rendered to the system layout"
else
    fail "unit paths were not rendered to the system layout"
fi

# ---------------------------------------------------------------------------
section "Profile selection"
for profile in hardware vm desktop; do
    if run_install "$TMP/install-$profile.log" --root "$TMP/profile-$profile" --profile "$profile"; then
        pass "install-system.sh --profile $profile"
    else
        fail "install-system.sh --profile $profile"
    fi
done
check_file "$TMP/profile-hardware/etc/udev/rules.d/70-ozma-node-hidg.rules"
check_file "$TMP/profile-hardware/etc/udev/rules.d/70-ozma-node-v4l2.rules"
check_absent "$TMP/profile-hardware/etc/udev/rules.d/70-ozma-node-uinput.rules"
check_file "$TMP/profile-vm/etc/udev/rules.d/70-ozma-node-uinput.rules"
check_file "$TMP/profile-vm/etc/udev/rules.d/70-ozma-node-input.rules"
check_absent "$TMP/profile-vm/etc/udev/rules.d/70-ozma-node-hidg.rules"
check_absent "$TMP/profile-vm/etc/udev/rules.d/70-ozma-node-v4l2.rules"
check_file "$TMP/profile-desktop/etc/udev/rules.d/70-ozma-node-uinput.rules"
check_absent "$TMP/profile-desktop/etc/udev/rules.d/70-ozma-node-hidg.rules"

# ---------------------------------------------------------------------------
section "Profile groups and udev contents"
check_profile_groups hardware "$TMP/profile-hardware/etc/ozma/install-operator-commands.txt"
check_profile_groups vm "$TMP/profile-vm/etc/ozma/install-operator-commands.txt"
check_profile_groups desktop "$TMP/profile-desktop/etc/ozma/install-operator-commands.txt"
check_profile_groups all "$R/etc/ozma/install-operator-commands.txt"

qmp_group=kvm
group_exists kvm || qmp_group=root
if grep -q 'GROUP="ozma-node"' "$R/etc/udev/rules.d/70-ozma-node-uinput.rules" &&
   grep -q 'GROUP="ozma-node"' "$R/etc/udev/rules.d/70-ozma-node-hidg.rules" &&
   grep -q 'GROUP="ozma-node"' "$R/etc/udev/rules.d/70-ozma-node-gadget.rules" &&
   grep -q 'GROUP="render"' "$R/etc/udev/rules.d/70-ozma-node-render.rules" &&
   grep -q 'GROUP="video"' "$R/etc/udev/rules.d/70-ozma-node-v4l2.rules" &&
   grep -q "GROUP=\"$qmp_group\"" "$R/etc/udev/rules.d/70-ozma-node-input.rules"; then
    pass "udev rules use the service, render, video, and QMP groups"
else
    fail "udev rule group rendering is inconsistent"
fi

# ---------------------------------------------------------------------------
section "Dry-run and toggles"
if run_install "$TMP/install-dry.log" --root "$TMP/dry" --dry-run --profile all --usb-gadget; then
    check_absent "$TMP/dry"
else
    fail "install-system.sh --dry-run"
fi

# A live-root dry run must work without root and still write nothing.
if run_install "$TMP/install-dry-live.log" --dry-run --profile hardware; then
    pass "install-system.sh --dry-run without --root (non-root)"
else
    fail "install-system.sh --dry-run without --root (non-root)"
fi

if run_install "$TMP/install-no-udev.log" --root "$TMP/no-udev" --profile all --no-udev; then
    check_absent "$TMP/no-udev/etc/udev/rules.d"
else
    fail "install-system.sh --no-udev"
fi

if run_install "$TMP/install-no-user.log" --root "$TMP/no-user" --profile hardware --no-user; then
    if grep -q 'groupadd' "$TMP/no-user/etc/ozma/install-operator-commands.txt" 2>/dev/null; then
        fail "--no-user still recorded groupadd"
    else
        pass "--no-user skipped account commands"
    fi
else
    fail "install-system.sh --no-user"
fi

# --no-env and --no-dirs must still leave the operator file readable, and must
# not silently create the skipped env/unit outputs.
MIN="$TMP/minimal"
if run_install "$TMP/install-minimal.log" --root "$MIN" --profile hardware \
        --no-user --no-env --no-dirs --no-units --no-udev --no-apparmor \
        --no-libvirt --no-tmpfiles --no-modules; then
    pass "install-system.sh composes --no-env with --no-dirs"
    check_file "$MIN/etc/ozma/install-operator-commands.txt"
    check_absent "$MIN/etc/ozma/ozma-node-hardware.env.example"
    check_absent "$MIN/usr/local/lib/systemd/system/ozma-node@.service"
else
    fail "install-system.sh composes --no-env with --no-dirs"
fi

# ---------------------------------------------------------------------------
section "Idempotence and --force"
before=$(find "$R" -type f -exec sha256sum {} + | sort)
if run_install "$TMP/install-second.log" --root "$R" --prefix /usr/local --profile all --usb-gadget; then
    after=$(find "$R" -type f -exec sha256sum {} + | sort)
    if [ "$before" = "$after" ]; then
        pass "second identical install is a no-op"
    else
        fail "second identical install changed files"
    fi
else
    fail "second identical install without --force"
fi

FR="$TMP/force"
if run_install "$TMP/install-force-base.log" --root "$FR" --profile hardware; then
    printf '# tamper\n' >>"$FR/etc/udev/rules.d/70-ozma-node-hidg.rules"
    if run_install_quiet "$TMP/install-force-refused.log" --root "$FR" --profile hardware; then
        fail "tampered file was accepted without --force"
    else
        pass "tampered file refused without --force"
    fi
    if run_install "$TMP/install-force-replace.log" --root "$FR" --profile hardware --force; then
        if grep -q '# tamper' "$FR/etc/udev/rules.d/70-ozma-node-hidg.rules"; then
            fail "--force did not replace the tampered rule"
        else
            pass "--force replaced the tampered rule"
        fi
    else
        fail "--force did not replace the tampered rule"
    fi
else
    fail "force-test base install"
fi

# ---------------------------------------------------------------------------
section "Custom service account and copy-only libvirt snippet"
CA="$TMP/custom-account"
if run_install "$TMP/install-custom-account.log" --root "$CA" --profile hardware \
        --user ozma-test --group ozma-test-grp; then
    account_ok=1
    for unit in ozma-node@.service ozma-node-vm@.service ozma-virtual-node.service; do
        if ! grep -q '^User=ozma-test$' "$CA/usr/local/lib/systemd/system/$unit" ||
           ! grep -q '^Group=ozma-test-grp$' "$CA/usr/local/lib/systemd/system/$unit"; then
            account_ok=0
        fi
    done
    grep -Eq '^d /run/ozma-node[[:space:]]+0750[[:space:]]+ozma-test[[:space:]]+ozma-test-grp[[:space:]]+-' \
        "$CA/etc/tmpfiles.d/ozma-node.conf" || account_ok=0
    grep -Eq '^d /run/ozma[[:space:]]+0755[[:space:]]+ozma-test[[:space:]]+ozma-test-grp[[:space:]]+-' \
        "$CA/etc/tmpfiles.d/ozma-node.conf" || account_ok=0
    grep -q 'GROUP="ozma-test-grp"' "$CA/etc/udev/rules.d/70-ozma-node-hidg.rules" || account_ok=0
    grep -q 'usermod -aG .* ozma-test$' "$CA/etc/ozma/install-operator-commands.txt" || account_ok=0
    if [ "$account_ok" -eq 1 ]; then
        pass "custom --user/--group rendered into units, tmpfiles, udev, and commands"
    else
        fail "custom --user/--group is inconsistent across rendered assets"
    fi
else
    fail "install-system.sh --user/--group"
fi

# An existing qemu.conf must never be edited; only the snippet is written.
LR="$TMP/libvirt-existing"
mkdir -p "$LR/etc/libvirt"
printf '# sentinel\ncgroup_device_acl = [ "/dev/null" ]\n' >"$LR/etc/libvirt/qemu.conf"
cp "$LR/etc/libvirt/qemu.conf" "$TMP/qemu.conf.before"
if run_install "$TMP/install-libvirt-existing.log" --root "$LR" --profile vm --no-user; then
    if cmp -s "$TMP/qemu.conf.before" "$LR/etc/libvirt/qemu.conf"; then
        pass "existing libvirt qemu.conf is unchanged"
    else
        fail "installer modified an existing libvirt qemu.conf"
    fi
    check_file "$LR/etc/ozma/libvirt-qemu-cgroup_device_acl.conf"
    if grep -q 'has cgroup_device_acl' "$TMP/install-libvirt-existing.log"; then
        pass "installer reports the existing cgroup_device_acl for manual merge"
    else
        fail "installer did not report the existing cgroup_device_acl"
    fi
else
    fail "install-system.sh with an existing libvirt qemu.conf"
fi

# ---------------------------------------------------------------------------
section "Secrets and permissions"
if grep -RInE '^[[:space:]]*OZMA_[A-Z_]*(TOKEN|SECRET|PASSWORD)[[:space:]]*=' "$R/etc/ozma" >/dev/null 2>&1; then
    fail "uncommented token/secret assignment in /etc/ozma"
else
    pass "no uncommented token/secret in /etc/ozma"
fi
if grep -RIl 'BEGIN .*PRIVATE KEY' "$R" >/dev/null 2>&1; then
    fail "private key material in the rendered tree"
else
    pass "no private key material in the rendered tree"
fi
if grep -RIn 'sudo' "$R" >/dev/null 2>&1; then
    fail "rendered file mentions sudo"
else
    pass "no sudo in rendered files"
fi

world_writable=$(find "$R" -perm -0002 -print -quit 2>/dev/null)
if [ -z "$world_writable" ]; then
    pass "no world-writable files or directories"
else
    fail "world-writable: ${world_writable#"$TMP"/}"
fi

check_mode "$R/etc/ozma" 750
check_mode "$R/etc/ozma/ozma-node-hardware.env.example" 640
check_mode "$R/etc/ozma/libvirt-qemu-cgroup_device_acl.conf" 640
check_mode "$R/usr/local/lib/systemd/system/ozma-node@.service" 644
check_mode "$R/etc/udev/rules.d/70-ozma-node-uinput.rules" 644
check_mode "$R/run/ozma-node" 750
check_mode "$R/run/ozma" 755
check_mode "$R/run/ozma/qmp" 770
check_mode "$R/var/lib/ozma-node" 750
check_mode "$R/var/lib/ozma" 750
check_absent "$R/etc/libvirt/qemu.conf"

# The tmpfiles snippet is applied at boot, so it must agree with the modes and
# owners the installer just created (especially /run/ozma, which the
# supervisor writes).
TMPFILES="$R/etc/tmpfiles.d/ozma-node.conf"
if grep -Eq '^d /run/ozma-node[[:space:]]+0750[[:space:]]+ozma-node[[:space:]]+ozma-node[[:space:]]+-$' "$TMPFILES" &&
   grep -Eq '^d /run/ozma[[:space:]]+0755[[:space:]]+ozma-node[[:space:]]+ozma-node[[:space:]]+-$' "$TMPFILES" &&
   grep -Eq "^d /run/ozma/qmp[[:space:]]+0770[[:space:]]+root[[:space:]]+$qmp_group[[:space:]]+-$" "$TMPFILES"; then
    pass "tmpfiles ownership matches the installer-created runtime dirs"
else
    fail "tmpfiles ownership disagrees with the installer-created runtime dirs"
fi

if grep -q 'systemd --user' "$R/etc/ozma/install-operator-commands.txt" &&
   grep -q 'Prefer a systemd --user unit' "$R/etc/ozma/ozma-node-desktop.env.example"; then
    pass "desktop system-vs-user caveat is documented"
else
    fail "desktop system-vs-user caveat is missing"
fi

# ---------------------------------------------------------------------------
section "Script safety"
for script in "$INSTALL" "$SCRIPT_DIR/install-rust.sh" "$VERIFY_UNITS"; do
    if grep -nE '(^|[;&|])[[:space:]]*sudo[[:space:]]' "$script" >/dev/null 2>&1; then
        fail "sudo invocation in ${script#"$REPO_ROOT"/}"
    else
        pass "no sudo invocation in ${script#"$REPO_ROOT"/}"
    fi
done

# ---------------------------------------------------------------------------
section "udev and AppArmor syntax"
if command -v udevadm >/dev/null 2>&1 && udevadm verify --help >/dev/null 2>&1; then
    udev_fail=0
    # -N never avoids failing on the not-yet-created ozma-node group. On an
    # older udevadm without that option, rewrite the group only in a temp copy
    # so the syntax check still does not need host account changes.
    if udevadm verify --help 2>&1 | grep -q -- '--resolve-names'; then
        udev_opts=(-N never)
    else
        udev_opts=()
    fi
    for rule in "$R"/etc/udev/rules.d/*.rules; do
        check_rule=$rule
        if [ "${#udev_opts[@]}" -eq 0 ]; then
            check_rule="$TMP/$(basename "$rule")"
            sed 's/GROUP="ozma-node"/GROUP="root"/g' "$rule" >"$check_rule"
        fi
        if ! udevadm verify "${udev_opts[@]}" "$check_rule" >/dev/null 2>&1; then
            fail "udevadm verify $(basename "$rule")"
            udev_fail=1
        fi
    done
    [ "$udev_fail" -eq 0 ] && pass "udevadm verify (all rules)"
else
    skip "udevadm verify is not available"
fi

if command -v apparmor_parser >/dev/null 2>&1; then
    wrapper="$TMP/ozma-apparmor-verify"
    {
        echo '#include <tunables/global>'
        echo 'profile ozma-verify /nonexistent {'
        echo '  #include <abstractions/base>'
        cat "$R/etc/apparmor.d/abstractions/libvirt-qemu.d/ozma"
        echo '}'
    } >"$wrapper"
    if apparmor_parser -Q "$wrapper" >/dev/null 2>"$TMP/apparmor.err"; then
        pass "apparmor_parser syntax check"
    else
        fail "apparmor_parser syntax check"
        cat "$TMP/apparmor.err" >&2
    fi
else
    skip "apparmor_parser is not available"
fi

# ---------------------------------------------------------------------------
section "systemd units (system and user)"
unit_out=$("$VERIFY_UNITS" 2>&1)
unit_rc=$?
if [ "$unit_rc" -ne 0 ]; then
    fail "verify-units.sh"
    printf '%s\n' "$unit_out" >&2
elif printf '%s\n' "$unit_out" | grep -q '^SKIP'; then
    skip "verify-units.sh ($unit_out)"
else
    pass "verify-units.sh (system + user)"
fi

# ---------------------------------------------------------------------------
section "Summary"
printf '%d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP"
if [ "$FAIL" -ne 0 ]; then
    echo "RESULT: FAIL" >&2
    exit 1
fi
echo "RESULT: PASS"
exit 0
