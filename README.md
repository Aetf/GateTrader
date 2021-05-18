# GateTrader

![license](https://img.shields.io/badge/license-MIT/Apache--2.0-blue)
[![linter](https://img.shields.io/github/workflow/status/Aetf/GateTrader/linter?label=linter)][workflow-linter]
[![tests](https://img.shields.io/github/workflow/status/Aetf/GateTrader/tests?label=tests)][workflow-tests]
[![codecov](https://img.shields.io/codecov/c/github/Aetf/GateTrader)][codecov]

Automatically sell coins on gate.io

```
gatetrader 0.1.0
Automatically sell coins on gate.io.

This program will load `.env` file from its working directory.

USAGE:
    gatetrader --key <key> --secret <secret> [ARGS]

OPTIONS:
    -h, --help
            Prints help information

    -k, --key <key>
            gate.io APIv4 key [env: KEY=b8ffbcfa3e8eafc345ade75f84c4a490]

    -s, --secret <secret>
            gate.io APIv4 secret [env: SECRET]

    -V, --version
            Prints version information


ARGS:
    <src-coin>
            currency pair source [default: ERG]

    <dst-coin>
            currency pair destination [default: USDT]
```

## License

Licensed under either of

 * Apache License, Version 2.0
   ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license
   ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

[codecov]: https://codecov.io/gh/GateTrader/GateTrader
[workflow-tests]: https://github.com/Aetf/GateTrader/actions/workflows/tests.yml
[workflow-linter]: https://github.com/Aetf/GateTrader/actions/workflows/linter.yml
