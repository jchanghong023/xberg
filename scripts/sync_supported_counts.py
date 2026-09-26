#!/usr/bin/env python3
"""Synchronize published format counts with the Rust MIME registry."""

from __future__ import annotations

import argparse
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Iterable, Sequence

REGISTRY_PATH = Path("crates/xberg/src/core/mime.rs")
MINIMUM_PRODUCT_FORMAT_COUNT = 50
MAXIMUM_PRODUCT_FORMAT_COUNT = 999
MINIMUM_PRODUCT_EXTENSION_COUNT = 50
MAXIMUM_PRODUCT_EXTENSION_COUNT = 999
NON_PUBLICATION_PATH_PARTS = frozenset(
    {
        "e2e",
        "test",
        "tests",
        "fixtures",
        "snapshot",
        "snapshots",
        "vendor",
        "vendored",
        "test_documents",
    }
)
GENERATED_PLUGIN_ROOTS = frozenset({".claude-plugin", ".codex-plugin", ".cursor-plugin", ".factory-plugin"})
# Everything under plugin/ outside .ai-rulez/ is generated output, and most of it
# carries no marker because ai-rulez writes JSON, which has no comment syntax. The
# marketplace README is the one hand-written source that lives in that tree: no
# generator writes it (a `--plugin` regen leaves it untouched) and it carries its own
# `~keep` note. Treating it as generated made `sync` skip it while `verify` still
# checked it, so the two could never agree and the count stayed stale at 106 through
# a full regen. ~keep
PLUGIN_SOURCE_PATHS = frozenset({("plugin", "README.md")})
GENERATED_HEADER_LINE_LIMIT = 5
PUBLICATION_TEXT_SUFFIXES = frozenset(
    {
        ".c",
        ".cs",
        ".dart",
        ".ex",
        ".exs",
        ".go",
        ".h",
        ".html",
        ".java",
        ".js",
        ".json",
        ".jsonc",
        ".kt",
        ".kts",
        ".md",
        ".mdx",
        ".mjs",
        ".php",
        ".py",
        ".rb",
        ".rs",
        ".rst",
        ".swift",
        ".toml",
        ".ts",
        ".tsx",
        ".txt",
        ".xml",
        ".yaml",
        ".yml",
        ".zig",
    }
)
PRODUCT_ATTRIBUTION = re.compile(
    r"\b(?:xberg|documents?|extract(?:s|ed|ing|ion)?|pdfs?|office|ocr|content[- ]intelligence)\b",
    re.IGNORECASE,
)
FORMAT_STRUCTURE = re.compile(r"\bformats\s+(?:across|including|with)\b|\bformats\s*[·(]", re.IGNORECASE)
DIRECT_SUPPORT_CLAIM = re.compile(r"\bSupports\s+\d+\s+(?:file|document) formats\b")
FORMAT_HEADLINE = re.compile(
    r"^\s*(?:\d+\.\s*)?\*\*\d+\s+(?:(?:file|document)\s+)?formats\*\*\s*:",
    re.IGNORECASE,
)
EXTENSION_HEADLINE = re.compile(r"\*\*\d+\s+file extensions\*\*", re.IGNORECASE)
EXTENSION_STRUCTURE = re.compile(
    r"\bComprehensive MIME type detection\s*\(\s*\d+\s+file extensions\s*\)",
    re.IGNORECASE,
)
CLAUSE_BOUNDARY = re.compile(r";|[!?](?=\s|$)|\.(?=\s+[A-Z]|\s*$)")
FORMAT_CLAIM = re.compile(r"(?<![\d~])(?P<count>\d+)\s+(?:(?:file|document)\s+)?formats\b", re.IGNORECASE)
EXTENSION_CLAIM = re.compile(r"(?<!\d)(?P<count>\d+)\s+file extensions\b", re.IGNORECASE)
REGISTRY_START = re.compile(r"\b(?:static|const)\s+FORMATS\s*:[^=]+?=\s*&\[")
ENTRY_START = re.compile(r"\bFormatEntry\s*\{")
ENTRY_FIELDS = re.compile(
    r"^\s*extensions\s*:\s*&\[(?P<extensions>.*?)\]\s*,\s*"
    r'mime_type\s*:\s*(?P<mime>"(?:[^"\\]|\\.)*"|[A-Z][A-Z0-9_]*)\s*,\s*'
    r"aliases\s*:\s*&\[(?P<aliases>.*?)\]\s*,?\s*$",
    re.DOTALL,
)
STRING_LITERAL = re.compile(r'"(?:[^"\\]|\\.)*"')


@dataclass(frozen=True)
class Counts:
    formats: int
    extensions: int


@dataclass(frozen=True)
class TextFile:
    path: Path
    content: str


@dataclass(frozen=True)
class Claim:
    start: int
    end: int
    line: int
    kind: str
    advertised: int


def _matching_delimiter(source: str, start: int, opening: str, closing: str) -> int:
    depth = 1
    index = start + 1
    in_string = False
    escaped = False
    while index < len(source):
        character = source[index]
        if in_string:
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == '"':
                in_string = False
        elif character == '"':
            in_string = True
        elif source.startswith("//", index):
            newline = source.find("\n", index + 2)
            index = len(source) if newline < 0 else newline
        elif source.startswith("/*", index):
            comment_end = source.find("*/", index + 2)
            if comment_end < 0:
                raise ValueError("unterminated block comment in FORMATS registry")
            index = comment_end + 1
        elif character == opening:
            depth += 1
        elif character == closing:
            depth -= 1
            if depth == 0:
                return index
        index += 1
    raise ValueError(f"unterminated {opening}{closing} block in FORMATS registry")


def _parse_string_array(source: str, field: str) -> list[str]:
    values: list[str] = []
    position = 0
    for match in STRING_LITERAL.finditer(source):
        if source[position : match.start()].strip(" \t\r\n,"):
            raise ValueError(f"malformed {field} array in FormatEntry")
        value = json.loads(match.group(0))
        if not isinstance(value, str) or not value:
            raise ValueError(f"{field} entries must be non-empty strings")
        values.append(value)
        position = match.end()
    if source[position:].strip(" \t\r\n,"):
        raise ValueError(f"malformed {field} array in FormatEntry")
    return values


def parse_registry(source: str) -> Counts:
    starts = list(REGISTRY_START.finditer(source))
    if len(starts) != 1:
        raise ValueError(f"expected exactly one FORMATS registry, found {len(starts)}")
    registry_start = starts[0].end() - 1
    registry_end = _matching_delimiter(source, registry_start, "[", "]")
    body = source[registry_start + 1 : registry_end]

    entry_starts = list(ENTRY_START.finditer(body))
    if not entry_starts:
        raise ValueError("FORMATS registry contains no FormatEntry values")

    extensions: set[str] = set()
    for entry_number, entry_start in enumerate(entry_starts, start=1):
        brace = entry_start.end() - 1
        entry_end = _matching_delimiter(body, brace, "{", "}")
        entry = body[brace + 1 : entry_end]
        fields = ENTRY_FIELDS.fullmatch(entry)
        if fields is None:
            raise ValueError(f"malformed FormatEntry #{entry_number}")
        entry_extensions = _parse_string_array(fields.group("extensions"), "extensions")
        _parse_string_array(fields.group("aliases"), "aliases")
        for extension in entry_extensions:
            if extension in extensions:
                raise ValueError(f"duplicate extension in FORMATS registry: {extension}")
            extensions.add(extension)

    return Counts(formats=len(entry_starts), extensions=len(extensions))


def _format_claim_is_product(line: str, advertised: int, attribution_text: str | None = None) -> bool:
    if advertised < MINIMUM_PRODUCT_FORMAT_COUNT or advertised > MAXIMUM_PRODUCT_FORMAT_COUNT:
        return False
    attribution_text = line if attribution_text is None else attribution_text
    return any(
        pattern.search(text) is not None
        for pattern, text in (
            (PRODUCT_ATTRIBUTION, attribution_text),
            (FORMAT_STRUCTURE, line),
            (DIRECT_SUPPORT_CLAIM, line),
            (FORMAT_HEADLINE, line),
        )
    )


def _extension_claim_is_product(line: str, advertised: int) -> bool:
    if advertised < MINIMUM_PRODUCT_EXTENSION_COUNT or advertised > MAXIMUM_PRODUCT_EXTENSION_COUNT:
        return False
    if (
        PRODUCT_ATTRIBUTION.search(line) is not None
        or EXTENSION_HEADLINE.search(line) is not None
        or EXTENSION_STRUCTURE.search(line) is not None
    ):
        return True
    return any(_format_claim_is_product(line, int(match.group("count"))) for match in FORMAT_CLAIM.finditer(line))


def _claim_clause(line: str, claim_start: int, claim_end: int) -> tuple[str, int]:
    boundaries = list(CLAUSE_BOUNDARY.finditer(line))
    start = max(
        (boundary.end() for boundary in boundaries if boundary.end() <= claim_start),
        default=0,
    )
    end = min(
        (boundary.start() for boundary in boundaries if boundary.start() >= claim_end),
        default=len(line),
    )
    return line[start:end], start


def _claims(text_file: TextFile) -> list[Claim]:
    claims: list[Claim] = []
    offset = 0
    lines = text_file.content.splitlines(keepends=True)
    for line_index, line in enumerate(lines):
        for match in FORMAT_CLAIM.finditer(line):
            advertised = int(match.group("count"))
            clause, clause_start = _claim_clause(line, match.start(), match.end())
            relative_start = match.start() - clause_start
            relative_end = match.end() - clause_start
            attribution_text = f"{clause[:relative_start]}{clause[relative_end:]}"
            if not _format_claim_is_product(clause, advertised, attribution_text):
                continue
            claims.append(
                Claim(
                    offset + match.start("count"),
                    offset + match.end("count"),
                    line_index + 1,
                    "formats",
                    advertised,
                )
            )
        for match in EXTENSION_CLAIM.finditer(line):
            advertised = int(match.group("count"))
            clause, _ = _claim_clause(line, match.start(), match.end())
            if not _extension_claim_is_product(clause, advertised):
                continue
            claims.append(
                Claim(
                    offset + match.start("count"),
                    offset + match.end("count"),
                    line_index + 1,
                    "file extensions",
                    advertised,
                )
            )
        offset += len(line)
    return sorted(claims, key=lambda claim: claim.start)


def _is_test_file(path: Path) -> bool:
    name = path.name.lower()
    return name.startswith("test_") or name.endswith("_test.py") or ".test." in name or ".spec." in name


def _is_publication_path(path: Path) -> bool:
    lowered_parts = {part.lower() for part in path.parts}
    if lowered_parts & NON_PUBLICATION_PATH_PARTS:
        return False
    if _is_test_file(path):
        return False
    return path.name.lower() not in {"changelog.md", "changelog.mdx"}


def _is_publication_surface(text_file: TextFile) -> bool:
    return _is_publication_path(text_file.path)


def _is_generated_output(text_file: TextFile) -> bool:
    header_end = 0
    for _ in range(GENERATED_HEADER_LINE_LIMIT):
        newline = text_file.content.find("\n", header_end)
        if newline == -1:
            header_end = len(text_file.content)
            break
        header_end = newline + 1
    lowercase_header = text_file.content[:header_end].lower()
    if (
        "alef:hash:" in text_file.content
        or "AI-RULEZ :: GENERATED FILE" in text_file.content
        or "generated by alef" in lowercase_header
    ):
        return True
    parts = text_file.path.parts
    if parts in PLUGIN_SOURCE_PATHS:
        return False
    if parts and parts[0] in GENERATED_PLUGIN_ROOTS:
        return True
    if len(parts) >= 2 and parts[:2] == (".agents", "plugins"):
        return True
    return len(parts) >= 2 and parts[0] == "plugin" and parts[1] != ".ai-rulez"


def _atomic_write(root: Path, relative_path: Path, content: str) -> None:
    resolved_root = root.resolve()
    if relative_path.is_absolute() or ".." in relative_path.parts:
        raise ValueError(f"refusing to write outside repository root: {relative_path}")
    destination = resolved_root / relative_path
    parent = destination.parent.resolve()
    if not parent.is_relative_to(resolved_root):
        raise ValueError(f"refusing to write outside repository root: {relative_path}")
    destination_stat = os.lstat(destination)
    if not stat.S_ISREG(destination_stat.st_mode):
        raise ValueError(f"refusing to replace non-regular file: {relative_path}")

    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(mode="wb", dir=parent, prefix=f".{destination.name}.", delete=False) as handle:
            temporary_path = Path(handle.name)
            handle.write(content.encode("utf-8"))
            handle.flush()
            os.fsync(handle.fileno())
        temporary_path.chmod(stat.S_IMODE(destination_stat.st_mode))
        temporary_path.replace(destination)
        temporary_path = None
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


def verify_files(counts: Counts, files: Iterable[TextFile]) -> tuple[list[str], int]:
    errors: list[str] = []
    advertisements = 0
    for text_file in files:
        if not _is_publication_surface(text_file):
            continue
        for claim in _claims(text_file):
            advertisements += 1
            expected = counts.formats if claim.kind == "formats" else counts.extensions
            if claim.advertised != expected:
                errors.append(
                    f"{text_file.path}:{claim.line}: advertises {claim.advertised} {claim.kind}; "
                    f"registry has {expected}"
                )
    if advertisements == 0:
        errors.append("no supported-format advertisements found in tracked UTF-8 files")
    return errors, advertisements


def synchronize_files(counts: Counts, files: Iterable[TextFile], root: Path) -> tuple[list[Path], int]:
    changed: list[Path] = []
    advertisements = 0
    for text_file in files:
        if not _is_publication_surface(text_file):
            continue
        content = text_file.content
        claims = _claims(text_file)
        advertisements += len(claims)
        if _is_generated_output(text_file):
            continue
        for claim in reversed(claims):
            expected = counts.formats if claim.kind == "formats" else counts.extensions
            content = f"{content[: claim.start]}{expected}{content[claim.end :]}"
        if content != text_file.content:
            _atomic_write(root, text_file.path, content)
            changed.append(text_file.path)
    if advertisements == 0:
        raise ValueError("no supported-format advertisements found in tracked UTF-8 files")
    return changed, advertisements


def tracked_text_files(root: Path) -> list[TextFile]:
    result = subprocess.run(
        ["git", "ls-files", "-s", "-z"],
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
    )
    files: list[TextFile] = []
    for record in result.stdout.split(b"\0"):
        if not record:
            continue
        try:
            metadata, raw_path = record.split(b"\t", 1)
            mode = metadata.split(b" ", 1)[0]
            path = Path(raw_path.decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as exception:
            raise ValueError("malformed path or index record from git ls-files") from exception
        if not _is_publication_path(path):
            continue
        if mode == b"120000":
            raise ValueError(f"refusing to read tracked symlink on a publication path: {path}")
        if mode not in {b"100644", b"100755"}:
            continue
        absolute = root / path
        try:
            working_tree_stat = os.lstat(absolute)
        except OSError as exception:
            raise ValueError(f"cannot inspect tracked publication file: {path}") from exception
        if not stat.S_ISREG(working_tree_stat.st_mode):
            raise ValueError(f"tracked publication working-tree file is not regular: {path}")

        descriptor: int | None = None
        try:
            descriptor = os.open(absolute, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
            opened_stat = os.fstat(descriptor)
            if not stat.S_ISREG(opened_stat.st_mode) or (
                opened_stat.st_dev != working_tree_stat.st_dev or opened_stat.st_ino != working_tree_stat.st_ino
            ):
                raise ValueError(f"tracked publication working-tree file changed while opening: {path}")
            with os.fdopen(descriptor, "rb") as handle:
                descriptor = None
                raw_content = handle.read()
        except OSError as exception:
            raise ValueError(f"cannot safely open tracked publication file: {path}") from exception
        finally:
            if descriptor is not None:
                os.close(descriptor)

        try:
            content = raw_content.decode("utf-8")
        except UnicodeDecodeError as exception:
            if path.suffix.lower() in PUBLICATION_TEXT_SUFFIXES:
                raise ValueError(f"tracked publication file is invalid UTF-8: {path}") from exception
            continue
        if "\0" in content:
            if path.suffix.lower() in PUBLICATION_TEXT_SUFFIXES:
                raise ValueError(f"tracked publication file contains NUL bytes: {path}")
            continue
        files.append(TextFile(path, content))
    return files


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("sync", "verify"))
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--registry", type=Path, default=REGISTRY_PATH)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    arguments = _parser().parse_args(argv)
    root = arguments.root.resolve()
    try:
        registry = arguments.registry if arguments.registry.is_absolute() else root / arguments.registry
        counts = parse_registry(registry.read_text(encoding="utf-8"))
        files = tracked_text_files(root)
        if arguments.mode == "sync":
            changed, advertisements = synchronize_files(counts, files, root)
            print(
                f"synchronized {advertisements} advertisements in {len(changed)} files "
                f"to {counts.formats} formats / {counts.extensions} file extensions"
            )
            for path in changed:
                print(path)
            return 0
        errors, advertisements = verify_files(counts, files)
    except (OSError, ValueError, subprocess.CalledProcessError) as exception:
        print(f"FAIL: {exception}", file=sys.stderr)
        return 2

    if errors:
        for message in errors:
            print(f"FAIL: {message}", file=sys.stderr)
        print(f"FAIL: checked {advertisements} advertisements", file=sys.stderr)
        return 1
    print(f"verified {advertisements} advertisements: {counts.formats} formats / {counts.extensions} file extensions")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
