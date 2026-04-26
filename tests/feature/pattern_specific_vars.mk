# pattern-specific vars
%.o: CFLAGS := -O2
%.c: SRCFLAGS := -pedantic

all: foo.o bar.o
	@echo done

foo.o:
	@echo "foo.o CFLAGS=$(CFLAGS)"

bar.o:
	@echo "bar.o CFLAGS=$(CFLAGS)"
