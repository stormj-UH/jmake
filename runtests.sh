#!/bin/bash
cat > /tmp/test_ab.mk << 'MEOF'
all: one.x two.x three.x
FOO = foo
BAR = bar
BAZ = baz
one.x: override FOO = one
%.x: BAR = two
t%.x: BAR = four
thr% : override BAZ = three
one.x two.x three.x: ; @echo $@: $(FOO) $(BAR) $(BAZ)
four.x: baz ; @echo $@: $(FOO) $(BAR) $(BAZ)
baz: ; @echo $@: $(FOO) $(BAR) $(BAZ)

# test matching multiple patterns
a%: AAA = aaa
%b: BBB = ccc
a%: BBB += ddd
%b: AAA ?= xxx
%b: AAA += bbb
.PHONY: ab
ab: ; @echo $(AAA); echo $(BBB)
MEOF

echo "=== Test all ==="
/build/jmake/target/release/jmake -f /tmp/test_ab.mk

echo ""
echo "=== Test ab ==="
/build/jmake/target/release/jmake -f /tmp/test_ab.mk ab

echo ""
echo "=== Test four.x ==="
/build/jmake/target/release/jmake -f /tmp/test_ab.mk four.x
