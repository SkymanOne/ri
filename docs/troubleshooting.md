# Troubleshooting

## Network

ri honors `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY`, and the `httpProxy` setting. Certificates are checked against the operating system's trust store.

`--offline`, or `PI_OFFLINE=1`, skips startup network work such as downloading `fd` and `rg`.

## No models available

ri found no credentials. Set a provider's API key or run `/login`, as described in [Models and sign-in](models.md).

## An extension breaks startup

`ri -ne` starts without extensions. `ri config` can then turn the failing one off.

## Reporting a problem

`/debug` writes what ri rendered and sent to `~/.ri/agent/ri-debug.log`. Include it in a report on the [issue tracker](https://github.com/SkymanOne/ri/issues), with the output of `ri --version` and your operating system.
