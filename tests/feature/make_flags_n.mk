# make -r (no builtin rules), -k (keep going), -n (dry run)
# Test -n: just print commands, don't execute
SRCS := foo.c bar.c
OBJS := $(patsubst %.c,%.o,$(SRCS))

all: $(OBJS)
	@echo "linking"

%.o: %.c
	cc -c $< -o $@
