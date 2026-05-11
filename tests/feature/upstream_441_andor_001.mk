NEQ = $(subst $1,,$2)
f =
t = true

all:
	@echo 1 $(and    ,$t)
	@echo 2 $(and $t)
	@echo 3 $(and $t,)
	@echo 4 $(and z,true,$f,false)
	@echo 5 $(and $t,$f,$(info bad short-circuit))
	@echo 6 $(and $(call NEQ,a,b),true)
	@echo 7 $(and $(call NEQ,a,a),true)
	@echo 8 $(and z,true,fal,se) hi
	@echo 9 $(and ,true,fal,se)there
	@echo 10 $(and   $(e) ,$t)