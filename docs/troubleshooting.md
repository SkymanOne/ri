# Troubleshooting

## Network

yapi honors `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY`, and the `httpProxy` setting. Certificates are checked against the operating system's trust store.

`--offline`, or `PI_OFFLINE=1`, skips startup network work such as downloading `fd` and `rg`.

## No models available

yapi found no credentials. Set a provider's API key or run `/login`, as described in [Models and sign-in](models.md).

## An extension breaks startup

`yapi -ne` starts without extensions. `yapi config` can then turn the failing one off.

## Reporting a problem

`/debug` writes what yapi rendered and sent to `~/.yapi/agent/yapi-debug.log`. Include it in a report on the [issue tracker](https://github.com/SkymanOne/yapi/issues), with the output of `yapi --version` and your operating system.
