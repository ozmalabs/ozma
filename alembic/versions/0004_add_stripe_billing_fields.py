"""Add Stripe billing fields to accounts table and create stripe_events table

Revision ID: 0004
Revises: 0003
Create Date: 2026-04-13 10:00:00.000000

"""
from alembic import op
import sqlalchemy as sa
from sqlalchemy.dialects import postgresql

# revision identifiers, used by Alembic.
revision = '0004'
down_revision = '0003'
branch_labels = None
depends_on = None


def upgrade() -> None:
    # Add columns to accounts table
    op.add_column('accounts', sa.Column('stripe_customer_id', sa.String(), nullable=True))
    op.add_column('accounts', sa.Column('plan', sa.String(), nullable=False, server_default='free'))
    op.add_column('accounts', sa.Column('plan_status', sa.String(), nullable=False, server_default='active'))
    op.add_column('accounts', sa.Column('plan_period_end', sa.DateTime(timezone=True), nullable=True))
    op.add_column('accounts', sa.Column('cancel_at_period_end', sa.Boolean(), nullable=False, server_default='false'))
    
    # Create stripe_events table
    op.create_table('stripe_events',
        sa.Column('event_id', sa.String(), nullable=False),
        sa.Column('event_type', sa.String(), nullable=False),
        sa.Column('processed_at', sa.DateTime(timezone=True), nullable=False),
        sa.PrimaryKeyConstraint('event_id')
    )
    
    # Create index on processed_at for performance
    op.create_index('ix_stripe_events_processed_at', 'stripe_events', ['processed_at'])


def downgrade() -> None:
    # Drop index
    op.drop_index('ix_stripe_events_processed_at', table_name='stripe_events')
    
    # Drop stripe_events table
    op.drop_table('stripe_events')
    
    # Drop columns from accounts table
    op.drop_column('accounts', 'cancel_at_period_end')
    op.drop_column('accounts', 'plan_period_end')
    op.drop_column('accounts', 'plan_status')
    op.drop_column('accounts', 'plan')
    op.drop_column('accounts', 'stripe_customer_id')
