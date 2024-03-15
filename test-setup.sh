#!/bin/bash

cargo install --locked --profile release-dbg --path binaries/geph5-client
cargo install --locked --profile release-dbg --target x86_64-unknown-linux-musl --path binaries/geph5-exit
cargo install --locked --profile release-dbg --target x86_64-unknown-linux-musl --path binaries/geph5-bridge
cargo install --locked --profile release-dbg --target x86_64-unknown-linux-musl --path binaries/geph5-broker

rsync -avz --progress $(which geph5-exit) root@c2.geph.io:/usr/local/bin
rsync -avz --progress $(which geph5-bridge) root@c2.geph.io:/usr/local/bin
ssh root@c2.geph.io systemctl restart geph5-exit

rsync -avz --progress $(which geph5-broker) root@binder.infra.geph.io:/usr/local/bin
ssh root@binder.infra.geph.io systemctl restart geph5-broker