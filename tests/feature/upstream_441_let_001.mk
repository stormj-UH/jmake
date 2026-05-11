a = bad
b = news
x = $(let a b,1 2,$a $b)
y = $(let a,1 2,$a)
z = $(let a b,1,$a $b)
all:;@echo 'a=,$a,' 'b=,$b,' 'x=,$x,' 'y=,$y,' 'z=,$z,'
