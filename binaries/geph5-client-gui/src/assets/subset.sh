#!/bin/bash
find ../ -type f -name '*.rs' -o -name '*.csv' | xargs cat | grep -o . | sort -u > chars.txt
pyftsubset IosevkaAile-Regular.ttf --text-file=chars.txt --output-file=normal.ttf
pyftsubset SarasaUiSC-Regular.ttf --text-file=chars.txt --output-file=chinese.ttf