# Disposable TLS fixture

The installed workspace verifier generates a fresh two-day test CA and localhost certificate in its temporary directory using the OpenSSL CLI. No private key is stored in the repository or distribution. Linux and macOS runners provide OpenSSL; Windows uses OpenSSL from Git for Windows when it is absent from PATH.

The package client receives the temporary CA explicitly and retains certificate verification. The verifier checks a real HTTPS package download, digest rejection and an HTTPS-to-HTTP redirect rejection.
