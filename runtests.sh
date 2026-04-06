#!/bin/bash
# Setup and run tests in a single docker session

apt-get install -y perl make curl wget 2>/dev/null | tail -3

cd /tmp
MAKE_VERSION=4.4.1
wget -q "https://ftp.gnu.org/gnu/make/make-${MAKE_VERSION}.tar.gz" -O make.tar.gz
tar xzf make.tar.gz
cd make-${MAKE_VERSION}
./configure --prefix=/usr/local 2>&1 | tail -2
make -j$(nproc) 2>&1 | tail -2
make install 2>&1 | tail -2
cp -r tests /tmp/make-tests
cd /tmp
rm -rf make-${MAKE_VERSION} make.tar.gz
echo "Test suite ready"

cd /tmp/make-tests
TEST="${1:-features/order_only}"
perl run_make_tests.pl -make /build/jmake/target/release/jmake -verbose $TEST 2>&1
EXIT=$?

echo ""
echo "=== Diff files ==="
for f in /tmp/make-tests/work/features/*.diff.*; do
    echo "--- $f ---"
    cat "$f"
done

exit $EXIT
