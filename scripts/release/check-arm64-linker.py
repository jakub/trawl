#!/usr/bin/env python3
"""Require Rust's ARM64 linker to patch a real Cortex-A53 erratum sequence.

The negative link leaves the aligned ADRP/load/load sequence intact. The Rust
link must patch it using the target's default --fix-cortex-a53-843419 argument.
This checks the release compiler driver, not a manually supplied workaround.
"""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def run(*args):
    try:
        return subprocess.check_output([str(arg) for arg in args], text=True, stderr=subprocess.STDOUT)
    except subprocess.CalledProcessError as error:
        sys.stderr.write(error.output)
        raise


def instructions(path):
    output = run("aarch64-linux-gnu-objdump", "-d", "--start-address=0x10ff8",
                 "--stop-address=0x11004", path)
    return [(int(address, 16), mnemonic) for address, mnemonic in
            re.findall(r"^\s*([0-9a-f]+):\s+[0-9a-f]{8}\s+(\w+)", output, re.MULTILINE)]


def check(directory):
    with tempfile.TemporaryDirectory(prefix="cortex-a53-", dir=directory) as temporary:
        work = Path(temporary)
        assembly = work / "erratum.s"
        assembly.write_text(""".section .text.erratum,\"ax\",%progbits
.balign 4096
.space 4088
.global _start
.type _start,%function
_start:
  adrp x0, probe_data
  ldr x1, [x1]
  ldr x0, [x0, :lo12:probe_data]
  ret
.size _start, .-_start
.section .data
probe_data: .quad 0
""")
        source = work / "probe.rs"
        source.write_text("""#![no_std]
#![no_main]
core::arch::global_asm!(include_str!("erratum.s"));
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
""")
        script = work / "layout.ld"
        script.write_text("ENTRY(_start)\nSECTIONS { .text 0x10000 : { *(.text.erratum) *(.text .text.*) } "
                          ".data 0x200000 : { *(.data .data.*) } }\n")
        target = "aarch64-unknown-linux-gnu"
        common = ["rustc", "--edition=2021", "--target", target, "-C", "panic=abort", str(source)]
        obj = work / "probe.o"
        run(*common, "--crate-type=lib", "--emit=obj", "-o", obj)
        unpatched = work / "unpatched"
        run("aarch64-linux-gnu-ld", "--no-relax", "-T", script, obj, "-o", unpatched)
        expected = [(0x10ff8, "adrp"), (0x10ffc, "ldr"), (0x11000, "ldr")]
        observed = instructions(unpatched)[:3]
        if observed != expected:
            raise SystemExit(f"ARM64 erratum fixture did not produce its unpatched negative witness: {observed}")
        patched = work / "patched"
        linker = os.environ.get("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER", "aarch64-linux-gnu-gcc")
        # Disable GCC's configured default so only Rust's explicit -Wl flag
        # can enable the patch. That linker flag takes priority over -mno-fix.
        run(*common, "-C", f"linker={linker}", "-C", "link-arg=-nostdlib",
            "-C", "link-arg=-no-pie", "-C", "link-arg=-Wl,--no-relax",
            "-C", "link-arg=-mno-fix-cortex-a53-843419",
            "-C", f"link-arg=-Wl,-T,{script}", "-o", patched)
        expected[2] = (0x11000, "b")
        observed = instructions(patched)[:3]
        if observed != expected:
            raise SystemExit(f"Rust's ARM64 linker did not apply the Cortex-A53 erratum mitigation: {observed}")
        print("ARM64 linker regression passed: negative sequence remains; Rust link patches its final load")


if __name__ == "__main__":
    check(Path(sys.argv[1]).resolve())
