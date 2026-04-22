# partest

Distributed cargo test runner. Runs `cargo nextest` across multiple machines on your local network using hash-based partitioning.

## How It Works

Each machine runs `partest daemon`, which advertises itself via mDNS. When you run `partest run` in a Rust project, the coordinating machine:

1. Discovers all peers on the local network
2. Builds a nextest archive (pre-compiled test binaries)
3. Distributes the archive and project source to every peer via SSH
4. Runs `cargo nextest run --partition hash:i/N` on each machine in parallel
5. Streams test output back in real-time

## Setup

### Prerequisites

Every machine that participates needs the following base setup.

#### 1. Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

#### 2. Install cargo-nextest

```bash
cargo install cargo-nextest --locked
```

#### 3. Install partest

```bash
git clone https://github.com/fabianboesiger/partest.git
cd partest
cargo install --path .
```

#### 4. Enable SSH

partest connects to peers via SSH with key-based authentication. Each machine needs an SSH server running.

**macOS:**

System Settings → General → Sharing → Remote Login → toggle ON

**Linux:**

```bash
sudo systemctl enable --now sshd
```

### For users who run tests (coordinator)

Coordinators run `partest run` to distribute tests. They need an SSH key to connect to all peers.

#### 1. Generate an SSH key

```bash
ssh-keygen -t ed25519
```

#### 2. Copy your public key to every peer (including yourself)

```bash
# For each peer machine:
ssh-copy-id user@peer-ip

# For the local machine:
cat ~/.ssh/id_ed25519.pub >> ~/.ssh/authorized_keys
```

#### 3. Verify passwordless SSH

```bash
ssh localhost
```

### For users who only run the daemon (worker)

Workers only need to run `partest daemon` and accept SSH connections from coordinators. They do **not** need to generate their own SSH key.

#### 1. Add coordinator public keys to authorized_keys

Get the public key (`~/.ssh/id_ed25519.pub`) from each coordinator and add it:

```bash
# Append each coordinator's public key:
echo "ssh-ed25519 AAAA... coordinator@host" >> ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
```

Alternatively, have the coordinator run:

```bash
ssh-copy-id user@this-machine
```

## Usage

### Start the daemon

Run this on every machine that should participate in testing:

```bash
partest daemon
```

This advertises the machine on the local network via mDNS. Keep it running in a terminal.

### Run tests

From any Rust project directory:

```bash
partest run
```

Options:

```
--release              Build and run tests in release mode
--ssh-user <USER>      SSH username (defaults to $USER)
--ssh-key <PATH>       SSH private key path (defaults to ~/.ssh/id_ed25519)
```

Extra arguments after `--` are forwarded to `cargo nextest run`:

```bash
partest run -- -E 'test(my_test)'
```

### Check peers

List all discovered machines on the network:

```bash
partest status
```

## Notes

- All machines must run the same OS and architecture (the archive contains pre-compiled binaries).
- mDNS uses multicast on UDP port 5353. Corporate firewalls may block this.
- Peers need `cargo-nextest` installed. partest does not install it automatically.
