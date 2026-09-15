# Local HTTPS package fixture

This public, disposable localhost certificate and key are used only by `scripts/verify-workspace.py`. The verifier explicitly adds this CA to its isolated distribution configuration, serves a temporary native package over HTTPS, and verifies checksum rejection and HTTPS downgrade rejection. Neither file is an application credential or a default trust root.
