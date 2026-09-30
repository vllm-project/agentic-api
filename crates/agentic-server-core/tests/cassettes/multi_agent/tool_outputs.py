PROPOSALS = {
    "alpha": {"estimated_weeks": 6, "risk": "medium"},
    "beta": {"estimated_weeks": 8, "risk": "low"},
}


def get_proposal(proposal):
    return PROPOSALS[proposal]
