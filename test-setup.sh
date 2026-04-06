#!/bin/bash
# Download GNU Make test suite (NOT source code - clean room)
# We only extract the test suite files and documentation

set -e

MAKE_VERSION=4.4.1
cd /tmp

# Download GNU Make tarball
curl -sL "https://ftp.gnu.org/gnu/make/make-${MAKE_VERSION}.tar.gz" -o make.tar.gz
tar xzf make.tar.gz

# Build GNU Make as a reference binary only
cd make-${MAKE_VERSION}
./configure --prefix=/usr/local 2>&1 | tail -3
# Build it (we need the binary for comparison testing)
make -j$(nproc) 2>&1 | tail -3
make install 2>&1 | tail -3

# Copy test suite to accessible location
cp -r tests /tmp/make-tests
cp -r doc /tmp/make-doc

# Clean up source (clean room - we should not have access to it)
cd /tmp
rm -rf make-${MAKE_VERSION}/src make-${MAKE_VERSION}/*.c make-${MAKE_VERSION}/*.h

echo "GNU Make ${MAKE_VERSION} installed and test suite extracted"
/usr/local/bin/make --version | head -1

# Verify test infrastructure
ls /tmp/make-tests/
