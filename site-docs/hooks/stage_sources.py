"""Put Markdown that lives elsewhere in the repository into the site.

The protocol specification (spec/) and the fuzzing notes (fuzz/README.md) are
read where they are, by people browsing the repository, and are not copied
into site-docs/. At build time each one is added to the site as a generated
page, so the repository keeps a single copy of every document.

Their relative links are written for the repository: `02-hashing.md`,
`vectors/`, `fuzz_targets/wallet.rs`. A link to another staged document is
pointed at that document's page; any other link is pointed at the file on
GitHub, where it resolves, rather than left to break under --strict.

Each staged page's "edit" button opens the source file, not a path under
site-docs/docs/ that does not exist.
"""

import os
import posixpath
import re

from mkdocs.structure.files import File

# Repository path -> site path. Order is the order nothing depends on; the nav
# in mkdocs.yml decides where each appears.
_SPEC_NAMES = [
    "00-architecture.md",
    "01-constants.md",
    "02-hashing.md",
    "03-crypto-primitives.md",
    "04-serialization.md",
    "05-addresses-keys-mnemonics.md",
    "06-transactions.md",
    "07-blocks-consensus.md",
    "08-p2p-protocol.md",
    "09-rpc-and-wallet-sync.md",
    "10-wallet.md",
    "11-storage.md",
    "12-roadmap.md",
]

STAGED = {"spec/README.md": "spec/index.md"}
STAGED.update({"spec/" + name: "spec/" + name for name in _SPEC_NAMES})
STAGED["spec/vectors/README.md"] = "spec/vectors.md"
STAGED["fuzz/README.md"] = "contributing/fuzzing.md"

# A directory whose README is staged links to that README's page.
_DIRS = {
    posixpath.dirname(src): dest
    for src, dest in STAGED.items()
    if posixpath.basename(src) == "README.md"
}

_REPO_ROOT = os.path.normpath(
    os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..")
)
_BRANCH = "development"

# [text](target) or [text](target "title"); the target has no whitespace.
_LINK = re.compile(r"(\]\()([^)\s]+)((?:\s+\"[^\"]*\")?\))")
_FENCE = re.compile(r"^\s*(```|~~~)")

# site path -> repository path, for the edit button.
_SOURCE_OF = {dest: src for src, dest in STAGED.items()}


def _github(repo_path, is_dir, fragment, config):
    kind = "tree" if is_dir else "blob"
    url = "%s/%s/%s/%s" % (config["repo_url"].rstrip("/"), kind, _BRANCH, repo_path)
    return url + fragment


def _rewrite_target(target, src, dest, config):
    if re.match(r"^[a-z][a-z0-9+.-]*:", target, re.I) or target.startswith("#"):
        return target

    path, hashmark, fragment = target.partition("#")
    fragment = hashmark + fragment
    trailing = path.endswith("/")
    resolved = posixpath.normpath(posixpath.join(posixpath.dirname(src), path))

    page = STAGED.get(resolved) or _DIRS.get(resolved)
    if page:
        rel = posixpath.relpath(page, posixpath.dirname(dest))
        return rel + fragment

    if resolved.startswith(".."):
        raise ValueError("%s links outside the repository: %s" % (src, target))

    on_disk = os.path.join(_REPO_ROOT, resolved.replace("/", os.sep))
    if not os.path.exists(on_disk):
        # Left alone, so the link check names it instead of GitHub 404ing.
        return target
    return _github(resolved, trailing or os.path.isdir(on_disk), fragment, config)


def _rewrite(markdown, src, dest, config):
    out = []
    in_fence = False

    for line in markdown.splitlines(keepends=True):
        if _FENCE.match(line):
            in_fence = not in_fence
        # Indented code as well as fenced: spec/ writes pseudocode such as
        # `HASHING_ALGORITHMS_BY_BLOCK_VERSION[major](powInput)` that way.
        if in_fence or line.startswith(("    ", "\t")):
            out.append(line)
            continue

        out.append(
            _LINK.sub(
                lambda m: m.group(1)
                + _rewrite_target(m.group(2), src, dest, config)
                + m.group(3),
                line,
            )
        )

    return "".join(out)


def on_files(files, config, **kwargs):
    for src, dest in STAGED.items():
        with open(os.path.join(_REPO_ROOT, src), encoding="utf-8") as handle:
            markdown = handle.read()

        files.append(
            File.generated(config, dest, content=_rewrite(markdown, src, dest, config))
        )

    return files


def on_page_markdown(markdown, page, config, **kwargs):
    src = _SOURCE_OF.get(page.file.src_uri)
    if src:
        page.edit_url = "%s/edit/%s/%s" % (config["repo_url"].rstrip("/"), _BRANCH, src)
    return markdown
