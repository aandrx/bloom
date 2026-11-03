#!/usr/bin/env python3
"""Regression tests for the Dart bindings refresh helper.

The helper is exercised only inside throwaway miniature checkouts. It is never
run against this repository's real lib/upstream tree.

Usage:
    python3 scripts/test-refresh-bindings.py [path-to-refresh_bindings.sh]

The optional helper path exists so a saved copy of a pre-fix helper can be run
to demonstrate a regression.
"""

import os
import shutil
import subprocess
import sys
import tempfile

DEFAULT_HELPER = os.path.join(
    "ext", "bloom-engine-sidecar", "dart", "tool", "refresh_bindings.sh"
)

HEADER = (
    b"// Modified for Bloom distribution: this notice was added; local formatting may differ.\n"
    b"// Upstream: OpenBubbles v1.15.0+205 (Apache-2.0); see sidecar ATTRIBUTION and\n"
    b"// lib/upstream/NOTICE.\n"
)

LICENSE_TEXT = b"Apache License\nVersion 2.0, January 2004\nlocal copy marker\n"
NOTICE_TEXT = b"Bloom engine sidecar NOTICE\nlocal attribution marker\n"

SOURCE_FILES = {
    "frb_generated.dart": "// fresh generated bindings\nclass Fresh {}\n",
    "frb_generated.io.dart": "// fresh io bindings\n",
    "lib.dart": "// fresh lib entry\n",
    "api/engine_api.dart": "// fresh engine api\n",
    "api/with space/api with space.dart": "// fresh spaced api\n",
    ".hidden_note.dart": "// fresh hidden file\n",
}

CURRENT_FILES = {
    "frb_generated.dart": "// current generated body\nclass Current {}\n",
    "frb_generated.io.dart": "// current io body",
    "lib.dart": "// current lib body\n",
    "api/api.dart": "// current api body\n",
    "api/api.freezed.dart": "// current freezed body\n",
}

REAL_MV = shutil.which("mv") or "/bin/mv"


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def snapshot(root):
    """Return {relative path: bytes or None for directories} for a tree."""
    entries = {}
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for dirname in dirnames:
            full = os.path.join(dirpath, dirname)
            entries[os.path.relpath(full, root)] = None
        for filename in sorted(filenames):
            full = os.path.join(dirpath, filename)
            with open(full, "rb") as handle:
                entries[os.path.relpath(full, root)] = handle.read()
    return entries


def write(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    mode = "wb" if isinstance(data, bytes) else "w"
    with open(path, mode) as handle:
        handle.write(data)


def make_checkout(base, helper_source):
    """Create <base>/mini checkout/dart with the helper under test."""
    checkout = os.path.join(base, "mini checkout")
    tool = os.path.join(checkout, "dart", "tool")
    os.makedirs(tool)
    helper = os.path.join(tool, "refresh_bindings.sh")
    shutil.copyfile(helper_source, helper)
    os.chmod(helper, 0o755)
    return checkout, helper


def make_source(base):
    """Create a fake upstream tag checkout, including spaces in paths."""
    source = os.path.join(base, "upstream checkout with spaces")
    rust = os.path.join(source, "lib", "src", "rust")
    for relative, text in SOURCE_FILES.items():
        write(os.path.join(rust, relative), text)
    return source


def make_destination(checkout, stale=True):
    dest = os.path.join(checkout, "dart", "lib", "upstream")
    write(os.path.join(dest, "LICENSE"), LICENSE_TEXT)
    write(os.path.join(dest, "NOTICE"), NOTICE_TEXT)
    if stale:
        write(os.path.join(dest, "frb_generated.dart"), "// stale generated\n")
        write(os.path.join(dest, "stale_deleted.dart"), "// stale extra\n")
        write(os.path.join(dest, "api", "old_api.dart"), "// stale nested\n")
    return dest


def make_current_destination(checkout):
    dest = make_destination(checkout)
    for relative, text in CURRENT_FILES.items():
        write(os.path.join(dest, relative), text)
    write(os.path.join(dest, "README.md"), "current non-Dart marker\n")
    write(os.path.join(dest, "notes", "keep.txt"), "current kept marker\n")
    return dest


def run_helper(helper, args, cwd, env=None):
    return subprocess.run(
        ["sh", helper] + args,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )


def check_metadata(dest):
    with open(os.path.join(dest, "LICENSE"), "rb") as handle:
        check(handle.read() == LICENSE_TEXT, "LICENSE was not preserved byte-for-byte")
    with open(os.path.join(dest, "NOTICE"), "rb") as handle:
        check(handle.read() == NOTICE_TEXT, "NOTICE was not preserved byte-for-byte")


def check_no_leftovers(checkout):
    leftovers = [
        name
        for name in os.listdir(os.path.join(checkout, "dart", "lib"))
        if name.startswith(".upstream")
    ]
    check(not leftovers, "refresh left staging or backup directories: %r" % leftovers)


def case_refresh_replaces_generated_and_keeps_metadata(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    source = make_source(base)
    dest = make_destination(checkout)
    source_snapshot = snapshot(os.path.join(source, "lib", "src", "rust"))

    result = run_helper(helper, [source], base)
    check(
        result.returncode == 0,
        "refresh failed: %s%s" % (result.stdout, result.stderr),
    )
    check_metadata(dest)

    dest_snapshot = snapshot(dest)
    for relative, data in source_snapshot.items():
        check(
            relative in dest_snapshot,
            "refreshed tree is missing generated entry: %s" % relative,
        )
        if data is None:
            check(dest_snapshot[relative] is None, "entry is not a directory: %s" % relative)
        elif relative.endswith(".dart"):
            check(
                dest_snapshot[relative] == HEADER + data,
                "fresh Dart file has a wrong or missing header: %s" % relative,
            )
        else:
            check(
                dest_snapshot[relative] == data,
                "refreshed content differs for: %s" % relative,
            )

    check(
        "stale_deleted.dart" not in dest_snapshot,
        "stale generated file was not removed",
    )
    check(
        os.path.join("api", "old_api.dart") not in dest_snapshot,
        "stale nested generated file was not removed",
    )
    check(
        dest_snapshot.get("frb_generated.dart")
        == HEADER + source_snapshot["frb_generated.dart"],
        "stale generated file survived instead of being replaced",
    )
    check_no_leftovers(checkout)


def case_missing_source_changes_nothing(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    make_source(base)
    dest = make_destination(checkout)
    before = snapshot(dest)

    missing = os.path.join(base, "does not exist")
    result = run_helper(helper, [missing], base)
    check(result.returncode != 0, "missing source path unexpectedly succeeded")
    check(
        snapshot(dest) == before,
        "destination changed even though the source path was missing",
    )


def case_refresh_rejects_empty_source(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    source = os.path.join(base, "empty upstream source")
    os.makedirs(os.path.join(source, "lib", "src", "rust"))
    dest = make_destination(checkout)
    before = snapshot(dest)

    result = run_helper(helper, [source], base)
    check(result.returncode != 0, "refresh accepted an empty source tree")
    check(
        "no regular .dart files" in result.stderr,
        "empty source failed without a clear no-Dart diagnostic",
    )
    check(
        snapshot(dest) == before,
        "destination changed even though the source had no Dart files",
    )
    check_no_leftovers(checkout)


def case_notice_current_rejects_empty_tree(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    dest = make_destination(checkout, stale=False)
    before = snapshot(dest)

    result = run_helper(helper, ["--notice-current"], base)
    check(result.returncode != 0, "notice-current accepted a tree with no Dart files")
    check(
        "no regular .dart files" in result.stderr,
        "empty current tree failed without a clear no-Dart diagnostic",
    )
    check(
        snapshot(dest) == before,
        "destination changed even though it had no Dart files",
    )
    check_no_leftovers(checkout)


def case_rejects_extra_source_arguments(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    source = make_source(base)
    dest = make_destination(checkout)
    before = snapshot(dest)

    result = run_helper(helper, [source, "extra"], base)
    check(result.returncode != 0, "extra positional arguments were accepted")
    check("usage:" in result.stderr, "extra arguments did not print usage")
    check(
        snapshot(dest) == before,
        "destination changed even though the invocation was rejected",
    )


def case_absent_destination_fails(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    source = make_source(base)
    dest = os.path.join(checkout, "dart", "lib", "upstream")

    result = run_helper(helper, [source], base)
    check(result.returncode != 0, "refresh succeeded without a destination")
    check(
        "missing local upstream directory" in result.stderr,
        "absent destination failed without a clear diagnostic",
    )
    check(not os.path.exists(dest), "absent destination was created unexpectedly")


def case_refresh_rejects_symlinked_source_metadata(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    source = make_source(base)
    dest = make_destination(checkout)
    outside = os.path.join(base, "upstream license target.txt")
    write(outside, "external marker\n")
    os.symlink(outside, os.path.join(source, "lib", "src", "rust", "LICENSE"))
    before = snapshot(dest)
    outside_before = open(outside, "rb").read()

    result = run_helper(helper, [source], base)
    check(
        open(outside, "rb").read() == outside_before,
        "refresh wrote through a symlink outside the staging directory",
    )
    check(result.returncode != 0, "refresh accepted a symlinked upstream LICENSE")
    check(
        "refusing symlink in source tree" in result.stderr,
        "symlink failure did not come from the source-tree preflight",
    )
    check(
        snapshot(dest) == before,
        "destination changed while refusing the source symlink",
    )


def case_missing_metadata_is_not_silently_accepted(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    source = make_source(base)
    dest = make_destination(checkout)
    os.remove(os.path.join(dest, "NOTICE"))
    before = snapshot(dest)

    result = run_helper(helper, [source], base)
    check(
        result.returncode != 0,
        "refresh succeeded even though the local NOTICE was missing",
    )
    check(
        snapshot(dest) == before,
        "destination changed while reporting a missing attribution file",
    )


def case_notice_current_adds_header_and_keeps_bodies(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    dest = make_current_destination(checkout)
    before = snapshot(dest)
    metadata_before = {
        name: before[name] for name in ("LICENSE", "NOTICE")
    }

    result = run_helper(helper, ["--notice-current"], base)
    check(
        result.returncode == 0,
        "notice-current failed: %s%s" % (result.stdout, result.stderr),
    )
    check_metadata(dest)

    after = snapshot(dest)
    for relative, text in CURRENT_FILES.items():
        body = text.encode()
        check(
            after[relative] == HEADER + body,
            "notice-current did not preserve the original bytes of: %s" % relative,
        )
    check(
        after["README.md"] == before["README.md"],
        "notice-current changed a non-Dart file",
    )
    check(
        after[os.path.join("notes", "keep.txt")] == before[os.path.join("notes", "keep.txt")],
        "notice-current removed or changed an existing non-Dart file",
    )
    for name, data in metadata_before.items():
        check(after[name] == data, "notice-current changed %s" % name)

    second = run_helper(helper, ["--notice-current"], base)
    check(
        second.returncode == 0,
        "second notice-current failed: %s%s" % (second.stdout, second.stderr),
    )
    check(
        snapshot(dest) == after,
        "repeated notice-current was not idempotent",
    )
    check_no_leftovers(checkout)


def case_notice_current_rejects_dart_symlink(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    dest = make_current_destination(checkout)
    outside = os.path.join(base, "outside target.txt")
    write(outside, "outside bytes\n")
    os.symlink(outside, os.path.join(dest, "escape.dart"))
    before = snapshot(dest)
    outside_before = open(outside, "rb").read()

    result = run_helper(helper, ["--notice-current"], base)
    check(result.returncode != 0, "notice-current accepted a symlinked Dart file")
    check(
        "refusing symlinked" in result.stderr,
        "symlink failure did not come from the staging annotation step",
    )
    check(
        open(outside, "rb").read() == outside_before,
        "notice-current wrote through a symlink outside the staged tree",
    )
    check(
        os.path.islink(os.path.join(dest, "escape.dart")),
        "destination changed while refusing the symlink",
    )
    check(
        snapshot(dest) == before,
        "destination changed even though a symlink was refused",
    )


def case_notice_current_rejects_symlinked_attribution(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    dest = make_current_destination(checkout)
    outside = os.path.join(base, "attribution target.txt")
    write(outside, "external attribution marker\n")
    os.remove(os.path.join(dest, "NOTICE"))
    os.symlink(outside, os.path.join(dest, "NOTICE"))
    before = snapshot(dest)
    outside_before = open(outside, "rb").read()

    result = run_helper(helper, ["--notice-current"], base)
    check(
        open(outside, "rb").read() == outside_before,
        "notice-current wrote through a symlinked attribution file",
    )
    check(result.returncode != 0, "notice-current accepted a symlinked NOTICE")
    check(
        "refusing symlinked destination attribution file" in result.stderr,
        "attribution symlink failure did not come from the destination check",
    )
    check(
        os.path.islink(os.path.join(dest, "NOTICE")),
        "destination attribution symlink was replaced",
    )
    check(
        snapshot(dest) == before,
        "destination changed while refusing an attribution symlink",
    )
    check_no_leftovers(checkout)


def case_rollback_failure_preserves_backup(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    source = make_source(base)
    dest = make_destination(checkout)
    shim = os.path.join(base, "shim")
    os.makedirs(shim)
    wrapper = os.path.join(shim, "mv")
    write(
        wrapper,
        "#!/bin/sh\n"
        "case \"$1\" in\n"
        "  */.upstream.refresh.*|*/.upstream.previous.*)\n"
        "    echo \"injected mv failure: $*\" >&2\n"
        "    exit 1\n"
        "    ;;\n"
        "esac\n"
        'exec %s "$@"\n' % REAL_MV,
    )
    os.chmod(wrapper, 0o755)
    env = dict(os.environ)
    env["PATH"] = shim + os.pathsep + env.get("PATH", "")

    result = run_helper(helper, [source], base, env=env)
    lib_dir = os.path.join(checkout, "dart", "lib")
    entries = os.listdir(lib_dir)
    backups = [name for name in entries if name.startswith(".upstream.previous.")]
    stagings = [name for name in entries if name.startswith(".upstream.refresh.")]
    check(result.returncode != 0, "injected swap failure unexpectedly succeeded")
    check(
        "failed to restore" in result.stderr
        and "backup preserved for recovery" in result.stderr,
        "rollback failure did not print the required diagnostic: %s" % result.stderr,
    )
    check(backups, "rollback failure deleted the preserved backup")
    check(not stagings, "rollback failure left staging behind")
    check(
        not os.path.exists(dest),
        "destination should be absent after the injected swap and rollback failure",
    )
    backup = os.path.join(lib_dir, backups[0])
    with open(os.path.join(backup, "LICENSE"), "rb") as handle:
        check(handle.read() == LICENSE_TEXT, "preserved backup lost its LICENSE")
    with open(os.path.join(backup, "NOTICE"), "rb") as handle:
        check(handle.read() == NOTICE_TEXT, "preserved backup lost its NOTICE")


def case_missing_metadata_blocks_notice_current(base, helper_source):
    checkout, helper = make_checkout(base, helper_source)
    dest = make_current_destination(checkout)
    os.remove(os.path.join(dest, "LICENSE"))
    before = snapshot(dest)

    result = run_helper(helper, ["--notice-current"], base)
    check(
        result.returncode != 0,
        "notice-current succeeded even though the local LICENSE was missing",
    )
    check(
        "missing local attribution file" in result.stderr,
        "missing metadata failure did not come from the attribution check",
    )
    check(
        snapshot(dest) == before,
        "notice-current changed the destination despite missing metadata",
    )


CASES = (
    case_refresh_replaces_generated_and_keeps_metadata,
    case_missing_source_changes_nothing,
    case_refresh_rejects_empty_source,
    case_notice_current_rejects_empty_tree,
    case_rejects_extra_source_arguments,
    case_absent_destination_fails,
    case_refresh_rejects_symlinked_source_metadata,
    case_missing_metadata_is_not_silently_accepted,
    case_notice_current_adds_header_and_keeps_bodies,
    case_notice_current_rejects_dart_symlink,
    case_notice_current_rejects_symlinked_attribution,
    case_rollback_failure_preserves_backup,
    case_missing_metadata_blocks_notice_current,
)


def main():
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    if len(sys.argv) > 2:
        print("usage: test-refresh-bindings.py [helper]", file=sys.stderr)
        return 2
    if len(sys.argv) == 2:
        helper_source = os.path.abspath(sys.argv[1])
    else:
        helper_source = os.path.join(repo_root, DEFAULT_HELPER)
    if not os.path.isfile(helper_source):
        print("helper not found: %s" % helper_source, file=sys.stderr)
        return 2

    failures = 0
    for case in CASES:
        base = tempfile.mkdtemp(prefix="bloom-refresh-bindings-")
        try:
            case(base, helper_source)
            print("ok: %s" % case.__name__)
        except (AssertionError, OSError) as error:
            failures += 1
            print("FAIL: %s: %s" % (case.__name__, error), file=sys.stderr)
        finally:
            shutil.rmtree(base, ignore_errors=True)

    if failures:
        print("%d refresh bindings case(s) failed" % failures, file=sys.stderr)
        return 1
    print("refresh bindings: all cases passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
