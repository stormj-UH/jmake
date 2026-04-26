# make -k: keep going after errors
all: foo bar baz

foo:
	@echo "foo ok"

bar:
	@false

baz:
	@echo "baz ok"
