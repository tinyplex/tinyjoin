ALTER TABLE "post_tags" DISABLE ROW LEVEL SECURITY;--> statement-breakpoint
DROP TABLE "post_tags" CASCADE;--> statement-breakpoint
ALTER TABLE "users" ADD COLUMN "visits" integer DEFAULT 0 NOT NULL;--> statement-breakpoint
CREATE INDEX "users_active_idx" ON "users" USING btree ("active");