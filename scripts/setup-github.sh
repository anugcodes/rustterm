#!/usr/bin/env bash
#
# Create the GitHub repository for this project (or connect to an existing
# one) and push the initial scaffold — WITHOUT requiring the GitHub CLI.
#
# Auth paths:
#   AUTH_MODE=ssh    (default) Uses your existing SSH key (git@github.com:...).
#                    Nothing else needed if your key is registered on GitHub.
#   AUTH_MODE=https  Uses HTTPS + a Personal Access Token (GITHUB_TOKEN).
#                    The token is passed via GIT_ASKPASS, never written to
#                    .git/config.
#
# Repo creation:
#   By default the repo must already exist on GitHub. Create it at:
#       https://github.com/new
#   (name it whatever REPO_NAME is; do NOT initialise it with a README).
#
#   Alternatively, set GITHUB_TOKEN and CREATE_REPO=1 and the script will
#   create the repo for you via the GitHub REST API using curl.
#
# Usage:
#   ./scripts/setup-github.sh [github-username] [repo-name]
#
# Environment:
#   AUTH_MODE      ssh | https            (default: ssh)
#   GITHUB_TOKEN   PAT — required for https, or for CREATE_REPO=1
#   CREATE_REPO    1 to create via API    (default: unset)
#   VISIBILITY     public | private       (default: public, API path only)
#   DEFAULT_BRANCH main                   (default: main)
#
# Examples:
#   # SSH, repo already created in the browser
#   ./scripts/setup-github.sh alice
#
#   # HTTPS with a classic PAT (scope: repo)
#   AUTH_MODE=https GITHUB_TOKEN=ghp_xxx ./scripts/setup-github.sh alice
#
#   # Let the script create a private repo via the API, then push over SSH
#   CREATE_REPO=1 VISIBILITY=private GITHUB_TOKEN=ghp_xxx \
#     ./scripts/setup-github.sh alice rustterm

set -euo pipefail

# ---------- Defaults ----------
DEFAULT_REPO_NAME="rustterm"
REPO_DESCRIPTION="Lightweight MobaXterm-style SSH client for Linux"
INITIAL_COMMIT_MSG="Initial scaffold: egui + russh + alacritty_terminal"

AUTH_MODE="${AUTH_MODE:-ssh}"
DEFAULT_BRANCH="${DEFAULT_BRANCH:-main}"
VISIBILITY="${VISIBILITY:-public}"
CREATE_REPO="${CREATE_REPO:-0}"

# ---------- Helpers ----------
err()  { printf 'error: %s\n' "$*" >&2; exit 1; }
info() { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }

# ---------- Preconditions ----------
command -v git  >/dev/null 2>&1 || err "git not found. Install: https://git-scm.com"
command -v curl >/dev/null 2>&1 || err "curl not found (needed for API repo creation)."
[[ -f Cargo.toml ]]            || err "Run from the project root (no Cargo.toml here)."

case "$AUTH_MODE"   in ssh|https)         : ;; *) err "AUTH_MODE must be ssh or https." ;; esac
case "$VISIBILITY"  in public|private)    : ;; *) err "VISIBILITY must be public or private." ;; esac

# ---------- Resolve owner / repo ----------
GH_USER="${1:-${GITHUB_USER:-}}"
REPO_NAME="${2:-$DEFAULT_REPO_NAME}"

if [[ -z "$GH_USER" ]]; then
  GH_USER="$(git config --global github.user 2>/dev/null || true)"
fi
if [[ -z "$GH_USER" ]]; then
  read -rp "GitHub username: " GH_USER
  [[ -n "$GH_USER" ]] || err "GitHub username is required."
fi
REPO_FULL="${GH_USER}/${REPO_NAME}"

info "user      : $GH_USER"
info "repo      : $REPO_NAME"
info "repo full : $REPO_FULL"
info "auth mode : $AUTH_MODE"
info "branch    : $DEFAULT_BRANCH"

# ---------- Git identity ----------
git config --get user.email >/dev/null 2>&1 \
  || err "git user.email not set. Run: git config --global user.email you@example.com"
git config --get user.name  >/dev/null 2>&1 \
  || err "git user.name not set.  Run: git config --global user.name 'Your Name'"

# ---------- Init repo ----------
if [[ ! -d .git ]]; then
  info "Initialising git repository on '$DEFAULT_BRANCH'."
  if ! git init -b "$DEFAULT_BRANCH" >/dev/null 2>&1; then
    git init >/dev/null
    git checkout -b "$DEFAULT_BRANCH"
  fi
fi

if ! git symbolic-ref --quiet --short HEAD >/dev/null 2>&1; then
  git checkout -b "$DEFAULT_BRANCH"
fi
CURRENT_BRANCH="$(git symbolic-ref --short HEAD)"

# ---------- Initial commit ----------
if ! git rev-parse --verify HEAD >/dev/null 2>&1; then
  info "Creating initial commit."
  git add -A
  if git diff --cached --quiet; then
    err "Nothing to commit. Is this the right directory?"
  fi
  git commit -m "$INITIAL_COMMIT_MSG" >/dev/null
fi

# ---------- Optional: create the repo via the GitHub API ----------
if [[ "$CREATE_REPO" == "1" ]]; then
  [[ -n "${GITHUB_TOKEN:-}" ]] || err "CREATE_REPO=1 requires GITHUB_TOKEN."

  code="$(curl -s -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer $GITHUB_TOKEN" \
    -H "Accept: application/vnd.github+json" \
    "https://api.github.com/repos/${REPO_FULL}")"

  case "$code" in
    200) info "Repo $REPO_FULL already exists." ;;
    404)
      info "Creating $REPO_FULL via the GitHub API."
      private_flag=false
      [[ "$VISIBILITY" == "private" ]] && private_flag=true
      payload="$(printf '{"name":"%s","private":%s,"description":"%s"}' \
        "$REPO_NAME" "$private_flag" "$REPO_DESCRIPTION")"
      curl -sf -X POST \
        -H "Authorization: Bearer $GITHUB_TOKEN" \
        -H "Accept: application/vnd.github+json" \
        -H "Content-Type: application/json" \
        -d "$payload" \
        "https://api.github.com/user/repos" >/dev/null \
        || err "API repo creation failed. Check that the PAT can create repos."
      ;;
    401|403)
      err "GitHub API auth failed (HTTP $code). PAT needs 'repo' scope (classic) or 'Administration: write' (fine-grained)."
      ;;
    *)
      err "GitHub API returned HTTP $code for $REPO_FULL."
      ;;
  esac
fi

# ---------- Remote ----------
if [[ "$AUTH_MODE" == "ssh" ]]; then
  REMOTE_URL="git@github.com:${REPO_FULL}.git"
else
  REMOTE_URL="https://github.com/${REPO_FULL}.git"
fi

if git remote get-url origin >/dev/null 2>&1; then
  cur="$(git remote get-url origin)"
  if [[ "$cur" != "$REMOTE_URL" ]]; then
    warn "Replacing origin: $cur -> $REMOTE_URL"
    git remote set-url origin "$REMOTE_URL"
  fi
else
  info "Adding remote origin -> $REMOTE_URL"
  git remote add origin "$REMOTE_URL"
fi

# ---------- Push (with diagnostics) ----------
push_diagnostics() {
  local log="$1"
  cat "$log" >&2
  if   grep -qi "repository not found" "$log"; then
    err "Repo ${REPO_FULL} doesn't exist. Create it at https://github.com/new, or rerun with CREATE_REPO=1 GITHUB_TOKEN=<PAT>."
  elif grep -qi "permission denied" "$log" || grep -qi "publickey" "$log"; then
    err "SSH auth failed. Add your key at https://github.com/settings/keys and test: ssh -T git@github.com"
  elif grep -qi "authentication failed" "$log"; then
    err "HTTPS auth failed. Check your PAT and its scopes."
  elif grep -qi "non-fast-forward" "$log" || grep -qi "\[rejected\]" "$log"; then
    err "Remote has commits you don't. Run: git pull --rebase origin $CURRENT_BRANCH, then rerun."
  fi
  err "Push failed (see output above)."
}

push_log="$(mktemp)"
trap 'rm -f "$push_log" "${askpass:-}"' EXIT

if [[ "$AUTH_MODE" == "https" ]]; then
  [[ -n "${GITHUB_TOKEN:-}" ]] || err "AUTH_MODE=https requires GITHUB_TOKEN."
  info "Pushing via HTTPS with PAT (token not written to .git/config)."

  askpass="$(mktemp)"
  cat >"$askpass" <<'ASK'
#!/usr/bin/env bash
case "$1" in
  *Username*) echo "x-access-token" ;;
  *Password*) echo "$GITHUB_TOKEN" ;;
esac
ASK
  chmod +x "$askpass"

  GIT_ASKPASS="$askpass" GITHUB_TOKEN="$GITHUB_TOKEN" \
    git push -u origin "$CURRENT_BRANCH" >"$push_log" 2>&1 \
    || push_diagnostics "$push_log"
else
  info "Pushing via SSH."
  git push -u origin "$CURRENT_BRANCH" >"$push_log" 2>&1 \
    || push_diagnostics "$push_log"
fi

info "Done: https://github.com/${REPO_FULL}"
