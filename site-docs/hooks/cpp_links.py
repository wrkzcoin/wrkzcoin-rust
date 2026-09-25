"""Link every citation of the C++ code to the line it cites.

spec/ and the guides cite the C++ reference as code spans: `src/crypto/hash.h`,
`Core.cpp:1586`, `ValidateTransaction.cpp:120-160`. All of them mean that file
at commit 8d89d7bf, the one spec/README.md pins. Each becomes a link to
https://github.com/wrkzcoin/wrkzcoin at that commit, with the line or range
highlighted, so a reader can check a claim against the code it came from.

A bare file name is resolved against cpp_files.txt, the C and C++ sources at
that commit. One that names no file there (this repository's own
`spec/vectors/blocks.cpp`, say) or more than one (`Constants.h`, in three
directories) is left as plain code rather than guessed at. `8d89d7bf` on its own links to the commit.
"""

import os
import re

COMMIT = "8d89d7bf92866e6f2985db149e49eb33eac61918"
_REPO = "https://github.com/wrkzcoin/wrkzcoin"


def _load():
    here = os.path.dirname(os.path.abspath(__file__))
    with open(os.path.join(here, "cpp_files.txt"), encoding="utf-8") as handle:
        return [l.strip() for l in handle if l.strip() and not l.startswith("#")]


_PATHS = _load()
_FULL = set(_PATHS)

# `file.ext`, `file.ext:12` or `file.ext:12-40`, not already a link's text.
_SPAN = re.compile(
    r"(?<![\[\w])`([A-Za-z0-9_./+-]+\.(?:c|cc|cpp|h|hpp|inl))(?::(\d+)(?:-(\d+))?)?`(?!\])"
)
_COMMIT_SPAN = re.compile(r"(?<![\[\w])`%s`(?!\])" % COMMIT[:8])
_FENCE = re.compile(r"^\s*(```|~~~)")

_resolved = {}


def _resolve(name):
    if name not in _resolved:
        if name in _FULL:
            _resolved[name] = name
        else:
            matches = [p for p in _PATHS if p.endswith("/" + name)]
            # `Transfer.cpp` and `Utilities.cpp` are also file names in the C++
            # command-line wallet, src/zedwallet++/. The documents cite that
            # front end by its full path, so a bare name means the library one.
            if len(matches) > 1:
                matches = [p for p in matches if not p.startswith("src/zedwallet++/")]
            _resolved[name] = matches[0] if len(matches) == 1 else None
    return _resolved[name]


def _link(match):
    path = _resolve(match.group(1))
    if path is None:
        return match.group(0)

    url = "%s/blob/%s/%s" % (_REPO, COMMIT, path)
    first, last = match.group(2), match.group(3)
    if first:
        url += "#L%s" % first + ("-L%s" % last if last else "")
    return "[%s](%s)" % (match.group(0), url)


def on_page_markdown(markdown, page, config, **kwargs):
    out = []
    in_fence = False

    for line in markdown.splitlines(keepends=True):
        if _FENCE.match(line):
            in_fence = not in_fence
        if in_fence or "`" not in line:
            out.append(line)
            continue

        line = _SPAN.sub(_link, line)
        line = _COMMIT_SPAN.sub(
            lambda m: "[%s](%s/commit/%s)" % (m.group(0), _REPO, COMMIT), line
        )
        out.append(line)

    return "".join(out)
