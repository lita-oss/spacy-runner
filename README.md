# spacy-runner

## Usage

```
spacy-runner [OPTIONS] [i|install] MODEL
spacy-runner [OPTIONS] [x|exec] PROG [ARG..]
```

CLI options / environment variables:

- `-p`/`--python`/`$PYTHON_VERSION`: the version string of CPython. Default: `"3.14"`.
- `-s`/`--spacy`/`$SPACY_VERSION`: the version string of spaCy. Default: `"3.8.18"`.
- `-u`/`--uv`/`$UV_BIN`: the path of the uv binary. Default: `"uv"`.
- `-v`/`--venv`/`$VIRTUAL_ENV`: the path of the venv directory. Default: `".venv"`.

## Examples

Download a spaCy model:

```
$ cargo run --release -- i en_core_web_sm
```

Execute a command:

```
$ cargo run --release -- x which python
.../bin/python
$ cargo run --release -- x python <<PY
import spacy
nlp = spacy.load("en_core_web_sm")
import en_core_web_sm
nlp = en_core_web_sm.load()
doc = nlp("This is a sentence.")
print([(w.text, w.pos_) for w in doc])
PY
```
