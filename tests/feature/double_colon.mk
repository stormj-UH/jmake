# double-colon rules execute in declared order
all:: first
	@echo "all rule 1"

all:: second
	@echo "all rule 2"

all:: third
	@echo "all rule 3"

first second third:
	@true
