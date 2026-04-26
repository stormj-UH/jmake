# order-only prerequisites
all: foo | bar
	@echo "all done"

foo:
	@echo "building foo"

bar:
	@echo "building bar"
