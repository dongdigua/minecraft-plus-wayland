#!/bin/sh

[ -f resources.zip ] || curl -O https://plus.minecraft.net/pkg/resources.zip
[ -f mcse_web_bg.wasm ] || curl -O https://plus.minecraft.net/pkg/mcse_web_bg.wasm

sha256sum -c <<EOF
68d45b56297688cc4abe9346012d1b9b26bf338bd419c8e427f4f92b98da18ab  resources.zip
ea6a8c953dc6702544bea01bb83620ec98098647c296c18bc78cb5ea80128608  mcse_web_bg.wasm
EOF
