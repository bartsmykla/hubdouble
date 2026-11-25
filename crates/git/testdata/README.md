# Test data

These files are live samples, or git's output for a made pair. They are not changed.

| File | Source | Licence |
|---|---|---|
| `explore-9d47da0-git-png.patch` | The `GIT binary patch` of `topics/git/git.png` in the patch of commit 9d47da0 of [github/explore](https://github.com/github/explore), as GitHub wrote it on 2026-11-24 | [CC-BY-4.0](https://github.com/github/explore/blob/main/LICENSE.txt), © GitHub |
| `vscode-codicons-45fc3f549ceb-codicon-old.ttf`, `vscode-codicons-45fc3f549ceb-codicon-new.ttf` | `dist/codicon.ttf` before and after commit 45fc3f549ceb of [microsoft/vscode-codicons](https://github.com/microsoft/vscode-codicons) (blobs b34cfdc and 373f406) | [CC-BY-4.0](https://github.com/microsoft/vscode-codicons/blob/main/LICENSE), © Microsoft |
| `delta-between-caps-old.bin`, `delta-between-caps-new.bin` | A made pair: 1600 random bytes that both share, then 64 random bytes in the old blob and 700 common English words in the new one (Python `random.Random(1)`). Its raw forward delta of 2966 bytes is larger than the new blob deflated at zlib level 6 (2941 bytes) and smaller than it deflated at level 1 (3036 bytes) | Made for hubdouble |
| `delta-between-caps-git.patch` | The `GIT binary patch` of that pair, as `git diff --no-index --binary` 2.47.3 wrote it | Made for hubdouble |
| `vscode-codicons-45fc3f549ceb-codicon.patch` | The `GIT binary patch` of `dist/codicon.ttf` in the patch of that commit, as GitHub wrote it on 2026-11-25 | [CC-BY-4.0](https://github.com/microsoft/vscode-codicons/blob/main/LICENSE), © Microsoft |
