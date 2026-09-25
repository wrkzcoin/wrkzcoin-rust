# Documentation

How this site is built, where each page comes from, and how a release is
published with its own archived copy. It is [MkDocs](https://www.mkdocs.org/)
with the [Material](https://squidfunk.github.io/mkdocs-material/) theme, the
same as the C++ site at [docs.wrkz.work](https://docs.wrkz.work/), from the
`site-docs/` directory of the repository.

## Where the pages come from

| Pages | Source |
| --- | --- |
| Everything but the two below | [`site-docs/docs/`](https://github.com/wrkzcoin/wrkzcoin-rust/tree/development/site-docs/docs) |
| Protocol Specification | [`spec/`](https://github.com/wrkzcoin/wrkzcoin-rust/tree/development/spec), and `spec/vectors/README.md` |
| Fuzzing | [`fuzz/README.md`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/fuzz/README.md) |

The specification and the fuzzing notes are not copied into `site-docs/`.
[`hooks/stage_sources.py`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/site-docs/hooks/stage_sources.py)
adds them to the site at build time and points their relative links either at
the other staged page or at the file on GitHub, so each is written once and
reads correctly in both places. The "edit" button on those pages opens the
original file.

[`hooks/llms_txt.py`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/site-docs/hooks/llms_txt.py),
taken from the C++ repository, writes `llms.txt`, `llms-full.txt` and a `.md`
copy of every page into the built site.

[`hooks/cpp_links.py`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/site-docs/hooks/cpp_links.py)
links every C++ file cited as a code span — `src/crypto/hash.h`,
`Core.cpp:1586`, `ValidateTransaction.cpp:120-160` — to that file and line in
[wrkzcoin/wrkzcoin](https://github.com/wrkzcoin/wrkzcoin) at `8d89d7bf`, the
commit the specification is pinned to. A bare file name is looked up in
`hooks/cpp_files.txt`, the C and C++ sources at that commit; a name that matches
no file there, or more than one, stays unlinked, so write the full path
(`src/walletbackend/Transfer.cpp`) when the bare name is ambiguous. If the
specification is ever re-pinned, regenerate that list as its header describes
and change `COMMIT` in the hook.

## Building it

Python 3.10 or later, and the pinned requirements in a virtualenv:

```sh
python3 -m venv .venv-docs
.venv-docs/bin/pip install -r site-docs/requirements.txt    # .venv-docs\Scripts\pip on Windows
source .venv-docs/bin/activate

mkdocs serve -f site-docs/mkdocs.yml            # http://127.0.0.1:8000, reloads on save
mkdocs build --strict -f site-docs/mkdocs.yml   # into site-docs/site/
```

`--strict` is what the `docs` workflow runs on every change to `site-docs/`,
`spec/` or `fuzz/README.md`: a link to a page or an anchor that does not exist,
or a page missing from the `nav` in `mkdocs.yml`, fails the build.

## Writing a page

- One `#` heading, then a paragraph whose first sentence says what the page is
  for. `llms.txt` uses that sentence as the page's description.
- Link to other pages by their `.md` path (`../node/configuration.md#anchor`),
  and to files in the repository by their GitHub URL on `development`.
- When a flag, a route, a default or a port changes, change the page that names
  it in the same commit.
- A new page goes in the `nav` in `mkdocs.yml`, or the strict build fails.

## Publishing, and versions

[`scripts/docs/publish.sh`](https://github.com/wrkzcoin/wrkzcoin-rust/blob/development/scripts/docs/publish.sh)
builds the site and syncs it to the web root that nginx serves as
`docs-rust.wrkz.work`:

```text
/var/www/docs-rust.wrkz.work/
    index.html  node/  wallets/  spec/  ...   the newest release, at the canonical URLs
    versions.json                             drives the version selector
    v1.0.1/                                   each release, as it shipped
    v1.0.0/
```

```sh
scripts/docs/publish.sh --dry-run          # what would change
scripts/docs/publish.sh                    # rebuild the live docs only
git checkout 1.0.1 && scripts/docs/publish.sh --archive       # a release: live docs and v1.0.1/
git checkout 1.0.0 && scripts/docs/publish.sh --archive-only  # archive an older tag, live docs untouched
```

The version comes from `Cargo.toml`, read the way `scripts/release.sh` reads it,
so the docs, the release archives and `wrkz-node --version` agree.
`versions.json` is rebuilt from the `v*/` directories actually on the server, so
the selector never offers a version that is not there; until the first
`--archive` it is absent and no selector is shown. `DOCS_WEB_ROOT` and
`DOCS_BASE_URL` override the defaults, for a staging host for example.
