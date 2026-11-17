# Lease-lane controller certificate

`lease-controller-ca.pem`, `lease-controller-server.pem` and `lease-controller-server.key.pem` are a throwaway test CA
and a server certificate whose subjectAltName is `controller.test`, used only by the no-spend lease-lane e2e. A lease's
guest network allow-list takes real DNS names (two labels or more), so the `localhost` certificate cannot serve it.

Made once with the system `openssl`, 100 years' validity:

    openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 36500 -subj "/CN=RyeOS Lease Test CA" \
      -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
    openssl req -newkey rsa:2048 -nodes -keyout server.key.tmp -out server.csr -subj "/CN=controller.test"
    printf 'subjectAltName=DNS:controller.test\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\n' > ext.cnf
    openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out server.pem -days 36500 -extfile ext.cnf
    openssl pkcs8 -topk8 -nocrypt -in server.key.tmp -out server.key.pem

The CA key was discarded. The server key is a public test fixture and protects nothing.
