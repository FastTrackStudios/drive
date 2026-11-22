# Vendoring copies each crate of a git dependency on its own, so an
# include_bytes!/include_str! that reaches out of its crate (pdfcraft's
# crates read fonts from the repository's assets/) finds nothing. Copy every
# such file into the crate (under _repo/) and point the macro at the copy.
# Run from the root of the git checkout.
import os, re, shutil, sys

root = os.getcwd()
macro = re.compile(r'(include_(?:bytes|str)!\(\s*")([^"]+)(")')

def crate_root(d):
    while d.startswith(root):
        if os.path.isfile(os.path.join(d, "Cargo.toml")):
            return d
        d = os.path.dirname(d)
    return None

for dirpath, dirs, files in os.walk(root):
    dirs[:] = [x for x in dirs if x not in (".git", "target")]
    for name in files:
        if not name.endswith(".rs"):
            continue
        path = os.path.join(dirpath, name)
        with open(path, encoding="utf-8", errors="surrogateescape") as f:
            text = f.read()
        crate = crate_root(dirpath)
        if crate is None:
            continue

        def fix(m):
            rel = m.group(2)
            if not rel.startswith(".."):
                return m.group(0)
            target = os.path.normpath(os.path.join(dirpath, rel))
            if target.startswith(crate + os.sep) or not os.path.exists(target):
                return m.group(0)
            if not target.startswith(root + os.sep):
                return m.group(0)
            dest = os.path.join(crate, "_repo", os.path.relpath(target, root))
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            if not os.path.exists(dest):
                shutil.copy2(target, dest)
            return m.group(1) + os.path.relpath(dest, dirpath) + m.group(3)

        new = macro.sub(fix, text)
        if new != text:
            with open(path, "w", encoding="utf-8", errors="surrogateescape") as f:
                f.write(new)
            print("inlined repo includes:", os.path.relpath(path, root), file=sys.stderr)
