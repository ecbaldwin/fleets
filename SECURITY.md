# Security Policy

## Supported versions

Security fixes are provided for the latest released version of fleets.

## Reporting a vulnerability

Please use GitHub's private vulnerability reporting feature for
`github.com/ecbaldwin/fleets`. Do not open a public issue for a suspected vulnerability.

Include reproduction steps, affected versions, impact, and any suggested mitigation. You can
expect an acknowledgement within seven days.

## Inventory trust boundary

Treat inventory sources as trusted input. Dynamic inventory scripts are executed directly and
inherit the fleets process environment. Fleets does not sandbox executable inventories or
restrict their network and filesystem access.
