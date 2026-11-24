#!/usr/bin/env bash
# Drive's releases: a CalVer tag (vYY.M.MICRO) on a commit of main. The tag
# is the release: pushing it starts .github/workflows/release.yml, which
# runs the full checks on THEBATTLESHIP's self-hosted runners (kache makes
# them warm) and, only when they pass, publishes the GitHub Release with
# the binaries. Nothing runs on pushes or pull requests (docs/release.md).
#
#   release.sh plan                  latest tag, what main holds since, the next version
#   release.sh notes [tag]           a tag's notes (default: the draft for origin/main)
#   release.sh release [--dry-run] [--yes] [sha]
#                                    tag origin/main (or sha, on main) as the next version
#   release.sh stamp X.Y.Z           write the version into this checkout (uncommitted)
#   release.sh guard                 in CI: the checkout is a stamped v* tag on main
#   release.sh publish DIR           in CI: the GitHub Release for the tag, with DIR's files
#   release.sh self-test             the version arithmetic, offline
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

die() { echo "release: $*" >&2; exit 1; }

# The highest release tag (no pre-release suffix), or nothing.
latest_tag() {
  git tag --list 'v*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1 || true
}

# The version after `$1` (a tag or empty) in month `$2` (YY.M).
next_after() {
  local last="${1#v}" month="$2"
  if [ -n "$last" ] && [ "${last%.*}" = "$month" ]; then
    echo "$month.$(( ${last##*.} + 1 ))"
  else
    echo "$month.0"
  fi
}

this_month() {
  # No zero padding (26.1, not 26.01): Cargo refuses a leading zero.
  echo "$(( $(date -u +%y) )).$(( 10#$(date -u +%m) ))"
}

# What a release since `$1` (a tag or empty) up to `$2` holds, by kind,
# from main's first-parent conventional commit titles (one per PR).
notes_between() {
  local from="$1" to="$2" range
  if [ -z "$from" ]; then
    echo "The first tagged release of Drive ($(git rev-list --first-parent --count "$to") changes on main)."
    return
  fi
  range="$from..$to"
  local subjects
  subjects="$(git log --first-parent --format='%s' "$range")"
  section() {
    local title="$1" pattern="$2" lines
    lines="$(printf '%s\n' "$subjects" | grep -E "$pattern" || true)"
    [ -n "$lines" ] || return 0
    printf '### %s\n\n' "$title"
    printf '%s\n' "$lines" | sed -E 's/^[a-z]+(\(([^)]*)\))?!?: /- **\2** /; s/- \*\*\*\* /- /'
    printf '\n'
  }
  section "Features" '^feat(\(|:|!)'
  section "Fixes" '^fix(\(|:|!)'
  section "Performance" '^perf(\(|:|!)'
  section "Other" '^(refactor|docs|chore|build|ci|test|style)(\(|:|!)'
  echo "Full list: https://github.com/FastTrackStudios/drive/compare/$from...$(git describe --tags --exact-match "$to" 2>/dev/null || git rev-parse --short "$to")"
}

cmd_plan() {
  git fetch -q origin main --tags
  local last next
  last="$(latest_tag)"
  next="$(next_after "$last" "$(this_month)")"
  echo "latest release: ${last:-none}"
  echo "committed version: $(workspace_version)"
  echo "next version:   v$next"
  echo "origin/main:    $(git log -1 --format='%h %s' origin/main)"
  echo
  notes_between "$last" origin/main
}

cmd_notes() {
  local tag="${1:-}"
  if [ -z "$tag" ]; then
    git fetch -q origin main --tags
    notes_between "$(latest_tag)" origin/main
    return
  fi
  git rev-parse -q --verify "refs/tags/$tag" >/dev/null || die "no tag $tag"
  local prev
  prev="$(git tag --list 'v*' --sort=-v:refname --merged "$tag^" | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1 || true)"
  notes_between "$prev" "$tag"
}

cmd_release() {
  local dry=false yes=false sha=""
  for a in "$@"; do
    case "$a" in
      --dry-run) dry=true ;;
      --yes) yes=true ;;
      -*) die "unknown flag $a" ;;
      *) sha="$a" ;;
    esac
  done
  git fetch -q origin main --tags
  sha="$(git rev-parse --verify "${sha:-origin/main}^{commit}")"
  git merge-base --is-ancestor "$sha" origin/main || die "$sha is not on origin/main"
  if git tag --points-at "$sha" | grep -qE '^v[0-9]'; then
    die "$sha is already released as $(git tag --points-at "$sha" | grep -E '^v[0-9]' | head -1)"
  fi
  local last next notes
  last="$(latest_tag)"
  if [ -n "$last" ] && ! git merge-base --is-ancestor "$last" "$sha"; then
    die "$sha is older than the latest release $last"
  fi
  next="v$(next_after "$last" "$(this_month)")"
  notes="$(notes_between "$last" "$sha")"
  echo "release $next at $(git log -1 --format='%h %s' "$sha")"
  echo
  echo "$notes"
  echo
  if $dry; then echo "(dry run: no tag)"; return; fi
  if ! $yes && [ -t 0 ]; then
    read -r -p "Tag and push $next? This starts the release build. [y/N] " ok
    [ "$ok" = y ] || [ "$ok" = Y ] || die "not tagged"
  fi
  git tag -a "$next" "$sha" -m "Drive ${next#v}" -m "$notes"
  git push origin "refs/tags/$next"
  echo "pushed $next: $(gh repo view --json url -q .url 2>/dev/null)/actions/workflows/release.yml"
}

workspace_version() {
  sed -n '/^\[workspace.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p}' Cargo.toml | head -1
}

cmd_stamp() {
  local v="${1:?stamp X.Y.Z}"
  [[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "not a version: $v"
  local old
  old="$(workspace_version)"
  # The members inherit [workspace.package]'s version; their Cargo.lock
  # entries must move with it, or `cargo --locked` refuses the lockfile.
  local members
  members="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[].name')"
  python3 - "$old" "$v" "$members" <<'PY'
import re, sys
old, new, members = sys.argv[1], sys.argv[2], set(sys.argv[3].split())
toml = open("Cargo.toml").read()
toml = re.sub(r'(\[workspace\.package\][^\[]*?\nversion = ")[^"]*(")', rf'\g<1>{new}\g<2>', toml, count=1)
open("Cargo.toml", "w").write(toml)
lock = open("Cargo.lock").read()
def entry(m):
    block = m.group(0)
    name = re.search(r'^name = "([^"]+)"', block, re.M).group(1)
    if name in members and 'source = ' not in block:
        block = re.sub(r'^version = "[^"]*"', f'version = "{new}"', block, count=1, flags=re.M)
    return block
lock = re.sub(r'\[\[package\]\]\n(?:(?!\n\n).)*', entry, lock, flags=re.S)
# A member that shares its name with another crate is referenced as
# "name version" in dependency lists.
for name in members:
    lock = lock.replace(f'"{name} {old}"', f'"{name} {new}"')
open("Cargo.lock", "w").write(lock)
PY
  echo "stamped $v (was $old)"
}

cmd_guard() {
  local tag="${GITHUB_REF_NAME:-$(git describe --tags --exact-match 2>/dev/null || true)}"
  [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "not on a release tag (${tag:-none})"
  git fetch -q origin main
  git merge-base --is-ancestor HEAD origin/main || die "$tag is not on main"
  [ "$(workspace_version)" = "${tag#v}" ] || die "the checkout says $(workspace_version), the tag ${tag#v}: stamp first"
  echo "guard: $tag on main, stamped"
}

cmd_publish() {
  local dir="${1:?publish DIR}"
  local tag="${GITHUB_REF_NAME:?publish runs on a tag}"
  local notes
  notes="$(git tag -l --format='%(contents:body)' "$tag")"
  [ -n "$notes" ] || notes="$(cmd_notes "$tag")"
  if gh release view "$tag" >/dev/null 2>&1; then
    gh release upload "$tag" "$dir"/* --clobber
  else
    gh release create "$tag" "$dir"/* --title "Drive ${tag#v}" --notes "$notes" --verify-tag
  fi
}

cmd_self_test() {
  [ "$(next_after "" 26.11)" = 26.11.0 ] || die "first release"
  [ "$(next_after v26.11.0 26.11)" = 26.11.1 ] || die "same month"
  [ "$(next_after v26.11.9 26.11)" = 26.11.10 ] || die "two digits"
  [ "$(next_after v26.11.3 26.12)" = 26.12.0 ] || die "new month"
  [ "$(next_after v26.12.2 27.1)" = 27.1.0 ] || die "new year"
  [[ "$(this_month)" =~ ^[0-9]{2}\.[1-9][0-9]?$ ]] || die "this month: $(this_month)"
  echo "release.sh: ok"
}

case "${1:-}" in
  plan) shift; cmd_plan "$@" ;;
  notes) shift; cmd_notes "$@" ;;
  release) shift; cmd_release "$@" ;;
  stamp) shift; cmd_stamp "$@" ;;
  guard) shift; cmd_guard "$@" ;;
  publish) shift; cmd_publish "$@" ;;
  self-test) shift; cmd_self_test "$@" ;;
  *) sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
