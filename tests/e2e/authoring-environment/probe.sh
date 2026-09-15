# Invoke with the pinned zsh -f; deliberately no ambient /bin/sh shebang.
set -eu
# Match root-worker executable search: only retained directory descriptors.
exec 9< /ryeos/realizations/authoring-tools/bin
export PATH=/proc/self/fd/9
export LC_ALL=C LANG=C TZ=UTC HOME=/project/probe
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_PAGER=cat
test ! -e /usr/bin
test ! -e /lib
cd /project/probe
test ! -e changed.txt
printf 'alpha\nbeta\n' > original.txt
cp original.txt changed.txt
sed -i 's/beta/gamma/' changed.txt
test "$(sed -n '2p' changed.txt)" = gamma
rg --quiet '^gamma$' changed.txt
grep -q '^alpha$' changed.txt
test "$(awk 'END { print NR }' changed.txt)" = 2
test "$(wc -l < changed.txt | tr -d ' ')" = 2
diff_status=0
diff -u original.txt changed.txt > edit.patch || diff_status=$?
test "$diff_status" = 1
cp original.txt patched.txt
patch patched.txt < edit.patch
cmp patched.txt changed.txt
find . -maxdepth 1 -name '*.txt' -print0 | xargs -0 wc -l > counts.txt
diff_status=0
git --no-pager diff --no-index -- original.txt changed.txt > git.patch || diff_status=$?
test "$diff_status" = 1
grep -q '^+gamma$' git.patch
mkdir scratch
git -C scratch init --template= --initial-branch=main
cp changed.txt scratch/result.txt
git -C scratch add result.txt
diff_status=0
git -C scratch --no-pager diff --cached --exit-code || diff_status=$?
test "$diff_status" = 1
test "$(git -C scratch status --porcelain)" = 'A  result.txt'
test "$(head -n 1 original.txt)" = alpha
test "$(tail -n 1 changed.txt)" = gamma
sha256sum changed.txt
printf 'authoring probe passed; changed.txt contains alpha/gamma\n'
