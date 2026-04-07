.SECONDEXPANSION:
.DEFAULT: ; @echo '$@'

all: foo bar baz


# Subtest #1
#
%oo: %oo.1; @:

foo: foo.2

foo: foo.3

foo.1: ; @echo '$@'


# Subtest #2
#
bar: bar.2

%ar: %ar.1; @:

bar: bar.3

bar.1: ; @echo '$@'


# Subtest #3
#
baz: baz.1

baz: baz.2

%az: ; @:
