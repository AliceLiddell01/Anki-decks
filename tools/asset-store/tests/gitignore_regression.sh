#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(git -C "$script_dir" rev-parse --show-toplevel)"
tmp_repo="$(mktemp -d)"
trap 'rm -rf -- "$tmp_repo"' EXIT

cp -- "$repo_root/.gitignore" "$tmp_repo/.gitignore"
git -C "$tmp_repo" init --quiet

make_file() {
  local path="$1"
  mkdir -p -- "$tmp_repo/$(dirname -- "$path")"
  : > "$tmp_repo/$path"
}

assert_ignored() {
  local path="$1"
  local status=0
  if git -C "$tmp_repo" check-ignore --quiet -- "$path"; then
    return 0
  else
    status=$?
  fi

  if [[ "$status" -eq 1 ]]; then
    printf 'ОШИБКА: путь должен игнорироваться: %s\n' "$path" >&2
  else
    printf 'ОШИБКА: git check-ignore завершился с кодом %s для %s\n' "$status" "$path" >&2
  fi
  return 1
}

assert_included() {
  local path="$1"
  local status=0
  if git -C "$tmp_repo" check-ignore --quiet -- "$path"; then
    printf 'ОШИБКА: путь не должен игнорироваться: %s\n' "$path" >&2
    return 1
  else
    status=$?
  fi

  if [[ "$status" -ne 1 ]]; then
    printf 'ОШИБКА: git check-ignore завершился с кодом %s для %s\n' "$status" "$path" >&2
    return 1
  fi
}

included_paths=(
  '.asset-store/kanji/.owner.json'
  '.asset-store/kanji/manifest.json'
  '.asset-store/kanji/assets/gif/漢.gif'
  '.asset-store/kanji/assets/png/饅.png'
  '.asset-store/pitch-accent/.owner.json'
  '.asset-store/pitch-accent/manifest.json'
  '.asset-store/pitch-accent/assets/png/幽霊.png'
)

ignored_paths=(
  '.asset-store/kanji/assets/gif/evil.gif/child'
  '.asset-store/kanji/assets/gif/evil.png/child'
  '.asset-store/kanji/assets/png/evil.png/child'
  '.asset-store/kanji/assets/png/evil.gif/child'
  '.asset-store/kanji/assets/gif/nested/file.gif'
  '.asset-store/kanji/assets/png/nested/deeper/file.png'
  '.asset-store/pitch-accent/assets/png/nested/file.png'
  '.asset-store/kanji/assets/gif/wrong-extension.png'
  '.asset-store/kanji/assets/png/wrong-extension.gif'
  '.asset-store/pitch-accent/assets/png/wrong-extension.jpg'
  '.asset-store/kanji/.runtime/candidates/manifest.json'
  '.asset-store/kanji/.tmp/transaction'
  '.asset-store/kanji/.lock'
  '.asset-store/pitch-accent/.runtime/candidates/manifest.json'
  '.asset-store/pitch-accent/.tmp/transaction'
  '.asset-store/pitch-accent/.lock'
  '.asset-store/kanji/unknown.txt'
)

for path in "${included_paths[@]}" "${ignored_paths[@]}"; do
  make_file "$path"
done

# Gitignore сопоставляет имена путей, поэтому whitelist проверяется на обычных
# файлах, а каталоги с тем же разрешённым суффиксом проверяются отдельно.
directory_paths=(
  '.asset-store/kanji/assets/gif/evil.gif'
  '.asset-store/kanji/assets/png/evil.png'
)
for path in "${directory_paths[@]}"; do
  mkdir -p -- "$tmp_repo/$path"
done

for path in "${included_paths[@]}"; do
  assert_included "$path"
done

for path in "${ignored_paths[@]}"; do
  assert_ignored "$path"
done

for path in "${directory_paths[@]}"; do
  assert_ignored "$path/"
done

printf 'Проверка .gitignore пройдена: %s разрешённых и %s игнорируемых путей, включая каталоги.\n' \
  "${#included_paths[@]}" "$(( ${#ignored_paths[@]} + ${#directory_paths[@]} ))"
