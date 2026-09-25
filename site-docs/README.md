# site-docs

The source of <https://docs-rust.wrkz.work/>, built with MkDocs Material as the
C++ site (<https://docs.wrkz.work/>) is.

    python3 -m venv .venv-docs && .venv-docs/bin/pip install -r site-docs/requirements.txt
    .venv-docs/bin/mkdocs serve -f site-docs/mkdocs.yml
    .venv-docs/bin/mkdocs build --strict -f site-docs/mkdocs.yml

| Path | What it is |
| --- | --- |
| `mkdocs.yml` | Site configuration and the navigation; every page must be listed there |
| `docs/` | The pages |
| `hooks/stage_sources.py` | Adds `spec/`, `spec/vectors/README.md` and `fuzz/README.md` as pages, so they are written once |
| `hooks/llms_txt.py` | Writes `llms.txt`, `llms-full.txt` and a `.md` copy of every page |
| `overrides/main.html` | The "not ready for production" bar on every page |
| `requirements.txt` | The exact versions the site is built with |

Publishing and versioned archives: `scripts/docs/publish.sh`, described on the
site's [Documentation](docs/contributing/documentation.md) page.
