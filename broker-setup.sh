#!/bin/bash

cargo install --locked  --target x86_64-unknown-linux-musl --path binaries/geph5-broker

rsync -avz --progress $(which geph5-broker) root@binder.infra.geph.io:/usr/local/bin
ssh root@binder.infra.geph.io systemctl restart geph5-broker