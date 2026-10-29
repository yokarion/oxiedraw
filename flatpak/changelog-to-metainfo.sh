#!/bin/sh
set -eu

root="$(dirname "$0")/.."
changelog=${1:-$root/docs/4.development/3.changelog.md}
metainfo_in=${2:-$root/data/com.yokarion.oxiedraw.metainfo.xml}
metainfo_out=${3:-$root/target/flatpak/com.yokarion.oxiedraw.metainfo.xml}

if grep -q '<releases' "$metainfo_in"; then
    echo "$metainfo_in already has <releases>; they come from the changelog" >&2
    exit 1
fi
if ! grep -qx '</component>' "$metainfo_in"; then
    echo "$metainfo_in has no </component> line" >&2
    exit 1
fi

releases=$(awk '
function escape(text) {
    gsub(/&/, "\\&amp;", text)
    gsub(/</, "\\&lt;", text)
    gsub(/>/, "\\&gt;", text)
    return text
}

function inline_markup(text,    count, parts, i, out) {
    count = split(escape(text), parts, "`")
    out = ""
    for (i = 1; i <= count; i++)
        out = out (i % 2 == 0 ? "<code>" parts[i] "</code>" : parts[i])
    return out
}

function flush(    i) {
    if (version != "" && item_count > 0) {
        print "    <release version=\"" version "\" date=\"" date "\">"
        print "      <description>"
        print "        <ul>"
        for (i = 1; i <= item_count; i++)
            print "          <li>" inline_markup(items[i]) "</li>"
        print "        </ul>"
        print "      </description>"
        print "    </release>"
    }
    version = ""
    item_count = 0
}

/^### / {
    flush()
    if ($0 ~ /^### v[^ ]+ \([0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]\) *$/) {
        version = substr($2, 2)
        date = substr($3, 2, 10)
    }
    next
}

/^## / { flush(); next }

version == "" { next }

/^- / {
    items[++item_count] = substr($0, 3)
    sub(/ +$/, "", items[item_count])
    next
}

/^  +[^ ]/ && item_count > 0 {
    line = $0
    sub(/^ +/, "", line)
    sub(/ +$/, "", line)
    items[item_count] = items[item_count] " " line
}

END { flush() }
' "$changelog")

if [ -z "$releases" ]; then
    echo "no dated releases found in $changelog" >&2
    exit 1
fi

mkdir -p "$(dirname "$metainfo_out")"
{
    grep -vx '</component>' "$metainfo_in"
    printf '  <releases>\n%s\n  </releases>\n</component>\n' "$releases"
} > "$metainfo_out"
