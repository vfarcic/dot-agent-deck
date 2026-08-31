#!/usr/bin/env python3
"""Rebuild a nextest JUnit report as METADATA ONLY, for safe artifact upload.

Issue #785. `.github/workflows/e2e-live.yml` is the one job in this repository
that holds an agent credential, and its nextest run writes
`target/nextest/default/junit.xml` from the cargo process that holds
`ANTHROPIC_API_KEY`. nextest's JUnit defaults store stdout AND stderr for failed
and retried tests, so if a test, an agent CLI, an HTTP error body, a panic
diagnostic or a contributor-authored test emits the key, the raw value lands in
that file. GitHub's secret masking covers log RENDERING; it does not cover files
copied out by `actions/upload-artifact`, and on a public repository such an
artifact is downloadable by any logged-in reader.

So this does not DELETE output from the report. It REBUILDS the document from a
per-element attribute whitelist, which is a stronger property: the result cannot
carry free text at all, because no code path here copies a text node, a tail, an
unlisted element or an unlisted attribute. A future nextest that adds a new
output-bearing element is dropped by default rather than passed through.

What survives is exactly what issue #564 wants the file for: test names,
outcomes, per-test wall clock, and the suite counts.

Usage:
    junit-strip-output.py <input.xml> <output.xml>
    junit-strip-output.py --self-test

A MISSING INPUT IS NOT AN ERROR (exit 0, nothing written). The workflow step
runs under `if: always()`, so it is reached on paths where the tests never ran
and no report exists; failing there would redden a job for a non-problem. The
upload step reads only the output path, so writing nothing means nothing is
uploaded rather than the raw file being uploaded as a fallback.
"""

from __future__ import annotations

import sys
import xml.etree.ElementTree as ET

# Element name -> the attributes that may be copied. Everything else — every
# other element, every other attribute, and every text node and tail in the
# document — is dropped.
#
# `message` is deliberately absent from `failure` / `error` / `skipped`: nextest
# puts the assertion text there, which is the same free text the element body
# carries. `type` is a fixed nextest string ("test failure", "test abort") and is
# kept because it is the only thing distinguishing a failure from an abort.
#
# `system-out`, `system-err`, `properties` and `property` are absent BY DESIGN:
# they are the output-bearing elements this whole script exists to remove.
ALLOWED: dict[str, frozenset[str]] = {
    "testsuites": frozenset(
        {"name", "tests", "failures", "errors", "skipped", "time", "timestamp", "uuid"}
    ),
    "testsuite": frozenset(
        {
            "name",
            "tests",
            "failures",
            "errors",
            "skipped",
            "time",
            "timestamp",
            "hostname",
            "id",
            "package",
        }
    ),
    "testcase": frozenset({"name", "classname", "time", "timestamp", "file", "line"}),
    "failure": frozenset({"type"}),
    "error": frozenset({"type"}),
    "skipped": frozenset({"type"}),
    # The retry elements carry their own `time`/`timestamp`, which is the one
    # place a FLAKY test's per-attempt wall clock is recorded — worth keeping,
    # and both are machine-formatted values rather than free text. Their
    # `message` and their nested `<system-out>`/`<system-err>`/`<stackTrace>`
    # are dropped like every other output surface.
    "flakyFailure": frozenset({"type", "time", "timestamp"}),
    "flakyError": frozenset({"type", "time", "timestamp"}),
    "rerunFailure": frozenset({"type", "time", "timestamp"}),
    "rerunError": frozenset({"type", "time", "timestamp"}),
}


class Stats:
    """Counts of what was dropped, so the CI log says what happened."""

    def __init__(self) -> None:
        self.dropped_elements: dict[str, int] = {}
        self.dropped_attributes: dict[str, int] = {}
        self.dropped_text_nodes = 0

    def note_element(self, tag: str) -> None:
        self.dropped_elements[tag] = self.dropped_elements.get(tag, 0) + 1

    def note_attribute(self, tag: str, attr: str) -> None:
        key = f"{tag}@{attr}"
        self.dropped_attributes[key] = self.dropped_attributes.get(key, 0) + 1

    def note_text(self) -> None:
        self.dropped_text_nodes += 1

    def summary(self) -> str:
        parts = []
        if self.dropped_elements:
            parts.append(
                "elements: "
                + ", ".join(
                    f"{tag} x{n}" for tag, n in sorted(self.dropped_elements.items())
                )
            )
        if self.dropped_attributes:
            parts.append(
                "attributes: "
                + ", ".join(
                    f"{key} x{n}" for key, n in sorted(self.dropped_attributes.items())
                )
            )
        if self.dropped_text_nodes:
            parts.append(f"text nodes: {self.dropped_text_nodes}")
        return "; ".join(parts) if parts else "nothing (report was already metadata-only)"


def rebuild(source: ET.Element, stats: Stats) -> ET.Element | None:
    """Return a metadata-only copy of `source`, or None if it is not whitelisted."""
    allowed = ALLOWED.get(source.tag)
    if allowed is None:
        stats.note_element(source.tag)
        return None

    if (source.text and source.text.strip()) or (source.tail and source.tail.strip()):
        stats.note_text()

    # A fresh element: no text, no tail, and only whitelisted attributes. Nothing
    # is carried over implicitly.
    clone = ET.Element(source.tag)
    for name, value in source.attrib.items():
        if name in allowed:
            clone.set(name, value)
        else:
            stats.note_attribute(source.tag, name)

    for child in source:
        rebuilt = rebuild(child, stats)
        if rebuilt is not None:
            clone.append(rebuilt)

    return clone


def strip_file(in_path: str, out_path: str) -> int:
    try:
        with open(in_path, "rb") as handle:
            tree = ET.parse(handle)
    except FileNotFoundError:
        print(
            f"junit-strip-output: no report at {in_path} — nothing to strip, "
            "nothing to upload."
        )
        return 0

    stats = Stats()
    root = rebuild(tree.getroot(), stats)
    if root is None:
        print(
            f"junit-strip-output: {in_path} has an unrecognised root element "
            f"<{tree.getroot().tag}>; refusing to guess. Nothing written.",
            file=sys.stderr,
        )
        return 1

    # Indent for a human reader: the artifact's whole purpose is being read
    # after a re-run has overwritten the conclusion, and nextest's own report is
    # indented. `ET.indent` is stdlib since 3.9.
    ET.indent(root, space="    ")
    ET.ElementTree(root).write(out_path, encoding="utf-8", xml_declaration=True)
    print(f"junit-strip-output: wrote {out_path}; dropped {stats.summary()}")
    return 0


# A synthetic report carrying every output-bearing shape nextest can emit, with a
# PLACEHOLDER credential (not a real key) standing in for a leak.
SELF_TEST_INPUT = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="2" failures="1" errors="0" time="1.5">
  <testsuite name="dot-agent-deck" tests="2" failures="1" errors="0">
    <testcase name="passing" classname="dot-agent-deck" time="0.1">
      <system-out>SENTINEL-PLACEHOLDER-NOT-A-KEY in stdout</system-out>
    </testcase>
    <testcase name="failing" classname="dot-agent-deck" time="1.4">
      <failure type="test failure" message="panicked: SENTINEL-PLACEHOLDER-NOT-A-KEY">
        thread 'failing' panicked with SENTINEL-PLACEHOLDER-NOT-A-KEY
      </failure>
      <system-err>SENTINEL-PLACEHOLDER-NOT-A-KEY in stderr</system-err>
      <properties>
        <property name="leak" value="SENTINEL-PLACEHOLDER-NOT-A-KEY"/>
      </properties>
    </testcase>
  </testsuite>
</testsuites>
"""

SENTINEL = "SENTINEL-PLACEHOLDER-NOT-A-KEY"


def self_test() -> int:
    """Prove the stripper removes every output shape, and keeps the metadata."""
    import tempfile
    from pathlib import Path

    failures: list[str] = []
    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp) / "junit.xml"
        dst = Path(tmp) / "out.xml"
        src.write_text(SELF_TEST_INPUT, encoding="utf-8")

        if strip_file(str(src), str(dst)) != 0:
            return 1
        text = dst.read_text(encoding="utf-8")

        if SENTINEL in text:
            failures.append("the placeholder credential survived the strip")
        for banned in ("system-out", "system-err", "properties", "property", "message="):
            if banned in text:
                failures.append(f"{banned} survived the strip")
        for wanted in ('name="passing"', 'name="failing"', 'type="test failure"', 'time="1.4"'):
            if wanted not in text:
                failures.append(f"metadata {wanted} was lost")

        # A missing input must be a no-op, not an error.
        missing = Path(tmp) / "absent.xml"
        out2 = Path(tmp) / "out2.xml"
        if strip_file(str(missing), str(out2)) != 0:
            failures.append("a missing input was treated as an error")
        if out2.exists():
            failures.append("a missing input still produced an output file")

    if failures:
        for line in failures:
            print(f"junit-strip-output self-test FAILED: {line}", file=sys.stderr)
        return 1
    print("junit-strip-output self-test: OK")
    return 0


def main(argv: list[str]) -> int:
    if len(argv) == 2 and argv[1] == "--self-test":
        return self_test()
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    return strip_file(argv[1], argv[2])


if __name__ == "__main__":
    sys.exit(main(sys.argv))
