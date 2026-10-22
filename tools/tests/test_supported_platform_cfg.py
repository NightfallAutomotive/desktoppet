"""Keep cfg predicates on the supported platform set and out of shared crates.

A temporary macOS/Windows compatibility union would make code compile for a
platform the contract test or release matrix is expected to catch, so it stays
forbidden everywhere.

Linux is staged by ADR-0090. A `target_os = "linux"` predicate is allowed only in
the crates that already own a per-platform branch (the platform adapters and the
product crates that select between them); the platform-neutral contract crates
must never learn about Linux, exactly as they never learn about macOS or Windows.
"""

import re
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
CRATES = ROOT / "crates"
CFG_ATTRIBUTE = re.compile(r"#\[cfg\s*\((.*?)\)\]", re.DOTALL)
ANY_EXPRESSION = re.compile(r"\bany\s*\((.*?)\)", re.DOTALL)
MACOS = 'target_os = "macos"'
WINDOWS = 'target_os = "windows"'
LINUX = 'target_os = "linux"'

# ADR-0090 "Platform boundary": the only crates a Linux cfg may appear in.
LINUX_CFG_CRATES = frozenset(
    {
        "bongocat-app",
        "bongocat-config",
        "bongocat-i18n",
        "bongocat-live2d",
        "bongocat-overlay",
        "bongocat-packaging",
        "bongocat-platform",
        "bongocat-ui",
        "bongocat-update",
    }
)


def crate_name(path, root=CRATES):
    """The workspace crate a source file belongs to, or None outside ``root``."""
    try:
        return path.relative_to(root).parts[0]
    except (ValueError, IndexError):
        return None


def scan_rust_file(path, linux_allowed=False):
    """Return ``(line_number, line, reason)`` for every forbidden cfg line."""
    source = path.read_text(encoding="utf-8")
    line_start = 1
    for match in CFG_ATTRIBUTE.finditer(source):
        expression = match.group(1)
        line_number = line_start + source[: match.start()].count("\n")
        line = source.splitlines()[line_number - 1]
        reasons = []
        if LINUX in expression and not linux_allowed:
            reasons.append("linux target_os outside an ADR-0090 adapter crate")
        for any_expression in ANY_EXPRESSION.finditer(expression):
            if MACOS in any_expression.group(1) and WINDOWS in any_expression.group(1):
                reasons.append("macOS+Windows any() union")
                break
        if reasons:
            yield line_number, line, "; ".join(reasons)
        line_start += source[match.start():match.end()].count("\n")


def offenders(root=CRATES):
    lines = []
    for path in sorted(root.rglob("*.rs")):
        linux_allowed = crate_name(path, root) in LINUX_CFG_CRATES
        for line_number, line, reason in scan_rust_file(path, linux_allowed):
            lines.append(f"{path}:{line_number}: {reason}: {line}")
    return lines


class SupportedPlatformCfgDetectorTests(unittest.TestCase):
    def test_forbidden_unions_and_linux_are_reported(self):
        cases = {
            "macos_windows.rs": '#[cfg(any(target_os = "macos", target_os = "windows"))]\n',
            "windows_macos.rs": '#[cfg(any(target_os = "windows", target_os = "macos"))]\n',
            "linux.rs": '#[cfg(target_os = "linux")]\n',
            "multiline.rs": '#[cfg(any(\n    target_os = "macos",\n    target_os = "windows",\n))]\n',
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, source in cases.items():
                (root / name).write_text(source, encoding="utf-8")

            reported = offenders(root)

        self.assertEqual(len(reported), len(cases))
        for name in cases:
            self.assertTrue(any(f"/{name}:1:" in line for line in reported), reported)

    def test_linux_cfg_is_allowed_only_in_adapter_crates(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for crate in ("bongocat-platform", "bongocat-runtime"):
                source = root / crate / "src"
                source.mkdir(parents=True)
                (source / "lib.rs").write_text(
                    '#[cfg(target_os = "linux")]\n'
                    '#[cfg(any(target_os = "macos", target_os = "windows"))]\n',
                    encoding="utf-8",
                )

            reported = offenders(root)

        self.assertEqual(
            sorted(line.split(": ", 2)[1] for line in reported),
            [
                "linux target_os outside an ADR-0090 adapter crate",
                "macOS+Windows any() union",
                "macOS+Windows any() union",
            ],
            reported,
        )
        self.assertTrue(
            all(
                "bongocat-runtime" in line
                for line in reported
                if "linux" in line
            ),
            reported,
        )

    def test_supported_cfgs_pass(self):
        source = "\n".join(
            (
                '#[cfg(target_os = "macos")]',
                '#[cfg(any(target_os = "windows", test))]',
                "#[cfg(all(target_os = \"macos\", target_arch = \"aarch64\"))]",
                "#[cfg(any(target_os = \"macos\", test))]",
            )
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "supported.rs"
            path.write_text(source, encoding="utf-8")
            self.assertEqual(offenders(Path(directory)), [])


class SupportedPlatformCfgContractTests(unittest.TestCase):
    def test_crates_contain_no_unsupported_platform_cfg(self):
        reported = offenders()
        self.assertEqual(reported, [], "\n".join(reported))


if __name__ == "__main__":
    unittest.main()
