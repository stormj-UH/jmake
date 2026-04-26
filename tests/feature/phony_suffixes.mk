# .PHONY and .SUFFIXES: clearing
.PHONY: all clean
.SUFFIXES:

all: clean
	@echo "all done"

clean:
	@echo "cleaning"

# This should NOT be an implicit rule since .SUFFIXES is cleared
%.o: %.c
	@echo "should not appear"
