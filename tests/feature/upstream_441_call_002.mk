all = $1 $2 $3 $4 $5 $6 $7 $8 $9

level1 = $(call all,$1,$2,$3,$4,$5)
level2 = $(call level1,$1,$2,$3)
level3 = $(call level2,$1,$2,$3,$4,$5)

all:
	@echo $(call all,1,2,3,4,5,6,7,8,9,10,11)
	@echo $(call level1,1,2,3,4,5,6,7,8)
	@echo $(call level2,1,2,3,4,5,6,7,8)
	@echo $(call level3,1,2,3,4,5,6,7,8)
