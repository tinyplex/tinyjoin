ALTER TABLE "users" ALTER COLUMN "name" DROP NOT NULL;--> statement-breakpoint
ALTER TABLE "users" ALTER COLUMN "active" SET DEFAULT false;--> statement-breakpoint
ALTER TABLE "users" ALTER COLUMN "visits" SET DATA TYPE bigint;--> statement-breakpoint
ALTER TABLE "users" DROP COLUMN "meta";