#!/bin/sh

set -eu

[ -f resources.zip ] || curl -O https://plus.minecraft.net/pkg/resources.zip
[ -f mcse_web_bg.wasm ] || curl -O https://plus.minecraft.net/pkg/mcse_web_bg.wasm

sha256sum -c <<EOF
68d45b56297688cc4abe9346012d1b9b26bf338bd419c8e427f4f92b98da18ab  resources.zip
ea6a8c953dc6702544bea01bb83620ec98098647c296c18bc78cb5ea80128608  mcse_web_bg.wasm
EOF

if [ ! -f lock/torch/redstone_torch.png ] \
    || [ ! -f lock/torch/soul_torch.png ] \
    || [ ! -f lock/torch/torch.png ] \
    || [ ! -f lock/torch/smooth_stone.png ] \
    || [ ! -f lock/torch/redstone_torch_off.png ]; then
    curl https://piston-data.mojang.com/v1/objects/37fd3c903861eeff3bc24b71eed48f828b5269c8/client.jar -o client_1.16.5.jar

    mkdir -p lock/torch
    unzip -jo client_1.16.5.jar \
        assets/minecraft/textures/block/redstone_torch.png \
        assets/minecraft/textures/block/soul_torch.png \
        assets/minecraft/textures/block/torch.png \
        assets/minecraft/textures/block/smooth_stone.png \
        assets/minecraft/textures/block/redstone_torch_off.png \
        -d lock/torch
fi

if [ ! -f lock/torch/copper_torch.png ]; then
    curl https://piston-data.mojang.com/v1/objects/ce92fd8d1b2460c41ceda07ae7b3fe863a80d045/client.jar -o client_1.21.9.jar
    mkdir -p lock/torch
    unzip -jo client_1.21.9.jar \
        assets/minecraft/textures/block/copper_torch.png \
        -d lock/torch
fi
