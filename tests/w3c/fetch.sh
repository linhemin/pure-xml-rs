#!/bin/sh
# Download the W3C XML 1.0 conformance suite (xmlts20130923) next to this script.
# The extracted tree is gitignored. Do not commit it.
set -eu
cd "$(dirname "$0")"
url="https://www.w3.org/XML/Test/xmlts20130923.zip"
archive="xmlts20130923.zip"
if [ ! -f "$archive" ]; then
  curl -fsSL -o "$archive" "$url"
fi
rm -rf xmlconf
unzip -q -o "$archive"
if [ ! -d xmlconf ] && [ -d xmlts20130923/xmlconf ]; then
  mv xmlts20130923/xmlconf xmlconf
  rm -rf xmlts20130923
fi
test -d xmlconf
echo "W3C suite ready at $(pwd)/xmlconf"
