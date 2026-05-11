# Simple, just reverse two things
#
reverse = $2 $1

# A complex 'map' function, using recursive 'call'.
#
map = $(foreach a,$2,$(call $1,$a))

# Test using a builtin; this is silly as it's simpler to do without call
#
my-notdir = $(call notdir,$(1))

# Test using non-expanded builtins
#
my-foreach = $(foreach $(1),$(2),$(3))
my-if      = $(if $(1),$(2),$(3))

# Test recursive invocations of call with different arguments
#
one = $(1) $(2) $(3)
two = $(call one,$(1),foo,$(2))

# Test recursion on the user-defined function.  As a special case make
# won't error due to this.
# Implement transitive closure using $(call ...)
#
DEP_foo = bar baz quux
DEP_baz = quux blarp
rest = $(wordlist 2,$(words ${1}),${1})
tclose = $(if $1,$(firstword $1)\
		$(call tclose,$(sort ${DEP_$(firstword $1)} $(call rest,$1))))

all: ; @echo '$(call reverse,bar,foo)'; \
        echo '$(call map,origin,MAKE reverse map)'; \
        echo '$(call my-notdir,a/b   c/d      e/f)'; \
        echo '$(call my-foreach)'; \
        echo '$(call my-foreach,a,,,)'; \
        echo '$(call my-if,a,b,c)'; \
        echo '$(call two,bar,baz)'; \
	echo '$(call tclose,foo)';
