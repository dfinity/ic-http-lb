# ic-http-lb

`ic-http-lb` is the simple HTTP load balancer that runs in front of HTTP Gateways on the [Internet Computer](https://internetcomputer.org).

## Description

`ic-http-lb` enables load balancing of incoming HTTP requests over a set of backends and performs TLS termination.

## Installation

To install and set up `ic-http-lb`, follow the steps below.

### Simple

- Grab the latest package from the [releases](https://github.com/dfinity/ic-http-lb/releases) page and install it
- Edit `/etc/default/ic-http-lb` file to configure the service using environment variables. See `Usage` section below.
- Start the service with `systemctl start ic-http-lb`

### Advanced

- **Clone the repository**

  ```bash
  git clone git@github.com:dfinity/ic-http-lb.git
  cd ic-http-lb
  ```

- **Install Rust**

  Follow the [official Rust installation guide](https://www.rust-lang.org/tools/install).

- **Build**
  
  Execute `cargo build --release` in the `ic-http-lb` folder and you'll get a binary in `target/release` subfolder.

### Reproducible build

 - Install [repro-env](https://github.com/kpcyrd/repro-env)
 - Build the binary using `repro-env build -- cargo build --release --target x86_64-unknown-linux-musl`

### Running in Docker
 - Pull the container: `docker pull ghcr.io/dfinity/ic-http-lb:latest`
 - Create the configuration file with the environment variables, e.g. `ic-http-lb.env`
 - Run the container: `docker run --env-file ic-http-lb.env ghcr.io/dfinity/ic-http-lb`

## Usage

### Minimal Example

To run a minimal ic-http-lb instance, use the following configuration in `/etc/default/ic-http-lb`:

```
CONFIG_PATH="/etc/ic-http-lb/config.yaml"
```

`config.yaml` example:
```
strategy: wrr

backends:
  - name: host1
    url: http://host1
    weight: 1
    enabled: true

  - name: host2
    url: http://host2
    weight: 2
    enabled: true
```

### Options

`ic-http-lb` offers various options that can be configured via command-line arguments or environment variables. For a full list, run `ic-http-lb --help`.

## Contributing

External code contributions are currently not being accepted to this repository.

## License

This project is licensed under the Apache License, Version 2.0. See the [LICENSE](LICENSE) file for more details.
